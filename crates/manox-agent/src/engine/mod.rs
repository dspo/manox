//! The pi harness engine: drives a `manox_harness::coding_agent::AgentSession` from a
//! tokio actor and adapts its events onto the UI's `ThreadEvent` language.
//!
//! This is the first-class harness backend behind the `Thread` facade: the
//! facade holds an `Arc<dyn ThreadEngine>` (this `PiEngine`), spawns the
//! actor, and drains `BackendNotice`s on the gpui thread. Pure mappings
//! between pi wire types and the UI language live here (adapt), so the
//! facade only ever sees `Message` / `ThreadEvent`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::subagent::SubagentRunObserver;
use manox_harness::bash::BashTool;
use manox_harness::bash::orchestration::BackgroundManager;
use manox_harness::bash::persistent::PersistentShellOperations;
use manox_harness::coding_agent::{AgentSession, ModelRuntime, create_agent_session};
use manox_harness::ext_point_agent::AgentRegistry;
use manox_harness::monitor::{MonitorManager, MonitorTool};
use manox_harness::subagent::spawn::register_defaults;
use manox_harness::subagent::{DelegationToolConfig, SpawnProvider, SubagentRuntime};
use manox_harness::tool::AgentTool as PiAgentTool;
use manox_harness::types::{AgentEvent, AgentMessage, ContentBlock, Model as PiModel};
use manox_harness::{BackgroundRegistry, BashOutputTool};
use tokio::sync::mpsc;

use crate::approval::{ApprovalGate, ApprovalGatedTool};
use crate::db::{HistoryEntry, PositionedNote, ThreadSummary, UI_NOTE_CUSTOM_TYPE, UiNoteRecord};
use crate::language_model::{MessageContent, ReasoningEffort, TokenUsage};
use crate::message::Message;
use crate::permission::{PendingAuthMeta, ToolAuthorizationResponse};
use crate::questions::PiAskUserQuestionTool;
use crate::thread::{PermissionMode, ThreadEvent};
use crate::thread_engine::{BackendNotice, ReadyInfo, SpawnedEngine, ThreadEngine};

// The engine is split by concern: journal plumbing, tool assembly,
// session lifecycle, and the run/settle loop. Each child starts from
// `use super::*`, and these globs bring the child items back into the
// engine scope for the glue below (and for the tests).
mod journal;
mod lifecycle;
mod run;
#[cfg(test)]
mod tests;
mod tools_assembly;
use journal::*;
use lifecycle::*;
use run::*;
use tools_assembly::*;
// Public aliases: `BrowserSuite` and the two journal entry points are
// addressed as `crate::engine::…` from other modules.
pub(crate) use journal::{dispatch_store_journal_row, register_engine_route};
pub use tools_assembly::BrowserSuite;

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
    /// A user plan-mode selection: recorded as a `plan_mode_request`
    /// journal entry and held pending until the next turn boundary commits it
    /// through `SetPlanMode`.
    RequestPlanMode { enabled: bool },
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

/// The feed's broadcast capacity: the Entry window the overflow-resync
/// semantics ride on. The value is declared here because the kernel is the
/// layer that owns the feed; a consumer that needs a bound states its own.
pub const JOURNAL_FEED_CAPACITY: usize = 4096;

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
    /// The user-questions seam's pending registry (the ask tool parks here;
    /// the gateway settles here through `Thread::respond_question`).
    question_gate: Arc<crate::questions::UserQuestionGate>,
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
    /// The cwd last reported through a `CwdChanged` event. Writers: the
    /// establishment announcement, the `SetCwd` handler, the post-`Ready`
    /// seed, and each turn's settle — one event per durable move, so the
    /// UI's directory display tracks the session tail without per-turn
    /// chatter.
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

impl PiEngine {
    /// Multi-root grant: approve an extra working directory into the
    /// session's shared granted-root set (the fs fence + seatbelt widen
    /// through the shared Arc, pre- or post-materialize).
    pub fn grant_working_directory(&self, dir: PathBuf) {
        self.state.granted_roots.approve(dir);
    }

    /// The session's granted roots beyond the workspace root.
    pub fn granted_working_directories(&self) -> Vec<PathBuf> {
        self.state.granted_roots.approved_roots()
    }
}

/// Spawn the pi actor and return the engine handle plus its notice receiver.
/// The facade drains the receiver on the gpui thread. `initial_path`, when
/// given, opens that session file instead of restoring the newest one.
/// `lease` is the driven session's write lease (None for fresh sessions —
/// a new id has no contender); the ACTOR holds it, not the facade: final
/// rows settle after the facade drops, and a lease outliving the actor
/// would keep a closed session locked against other processes.
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
    extra_granted_roots: &[PathBuf],
    lease: Option<std::sync::Arc<crate::session_lease::LeaseEntry>>,
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
    //
    // That guarantee is scoped to EMISSION sites: it covers everything sent
    // into `notice_tx`, which is upstream of the tap. A store-level decision
    // that consumes a notice at the facade (`ThreadHandle::handle_notice`)
    // is downstream of it and persists nothing, so such a decision must append
    // its own row via `dispatch_store_journal_row` — see `journal_pinned_archived`
    // and `journal_title` in `thread_store`.
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
    // The question seam owns its own pending registry and journals its verdicts
    // as `question` decision entries through the same actor queue.
    let question_gate = Arc::new(crate::questions::UserQuestionGate::new(notice_tx.clone()));
    question_gate.set_journal_sink(cmd_tx.clone());
    // The thread-scoped journal feed; session relays publish into it as
    // sessions come and go (capacity matches the storage broadcast, L5).
    let (journal_feed_handle, _) =
        tokio::sync::broadcast::channel::<JournalFeed>(JOURNAL_FEED_CAPACITY);
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
        question_gate,
        plan: crate::plan_mode::PlanSessionState::new(),
        goal_bridge,
        goal_continuation_reserved: AtomicBool::new(false),
        goal_continuation_round: Mutex::new(None),
        session_start_fired: AtomicBool::new(false),
        granted_roots: {
            let granted = crate::granted_roots::GrantedRoots::new(cwd.clone());
            for extra in extra_granted_roots {
                granted.approve(extra.clone());
            }
            granted
        },
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
        // Held for the whole actor lifetime — through run_actor's settle —
        // and released here, after the route is gone and the last row has
        // landed: only then may another process take the session over.
        let _lease = lease;
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
    fn grant_working_directory(&self, dir: PathBuf) {
        PiEngine::grant_working_directory(self, dir);
    }
    fn granted_working_directories(&self) -> Vec<PathBuf> {
        PiEngine::granted_working_directories(self)
    }
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

    fn steer(
        &self,
        text: String,
        images: Vec<manox_harness::types::ContentBlock>,
        message_id: Option<String>,
    ) -> String {
        let id = message_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
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
        // The gate is the live authority every tool resolver reads per call,
        // so the mode lands on it directly — a mid-turn switch governs the
        // very next tool call. The queued command still serializes the
        // durable half (sidecar persist + journal echo) on the actor and
        // re-stamps the same value idempotently.
        self.state.gate.set_mode(mode);
        let _ = self.cmd_tx.send(SessionCmd::SetPermissionMode(mode));
    }

    fn set_plan_mode(&self, enabled: bool) {
        let _ = self.cmd_tx.send(SessionCmd::RequestPlanMode { enabled });
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

    fn respond_question(&self, id: &str, outcome: crate::questions::AskOutcome) {
        self.state.question_gate.respond(id, outcome);
    }

    fn pending_question_entries(&self) -> Vec<(String, PendingAuthMeta)> {
        self.state.question_gate.pending_entries()
    }

    fn set_thinking_level(&self, level: Option<String>) {
        let _ = self.cmd_tx.send(SessionCmd::SetThinkingLevel(level));
    }

    fn open_session(&self, path: PathBuf) {
        let _ = self.cmd_tx.send(SessionCmd::Open { path });
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
        // Retire and land whatever was queued before the exit, exactly as the
        // command loop's own shutdown does. An early return that skipped this
        // left the route registered with a live sender, so a store dispatch
        // sent its row into an actor that would never drain it and reported
        // the row queued — the rename's sidecar then named a title whose
        // journal row had never been written.
        let stranded = retire_and_claim_journal_rows(&thread_id, &mut cmd_rx);
        if !stranded.is_empty() {
            tracing::debug!(
                thread = %thread_id,
                rows = stranded.len(),
                "engine exited without a model; landing its queued journal rows"
            );
            for (kind, payload) in stranded {
                cold_journal_append(initial_path.clone(), kind, payload).await;
            }
        }
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
        // An explicit open reads only the requested transcript's HEADER —
        // one line, microsecond scale. The store-wide `repo.list()` scan
        // walks every session file (bounded, but still O(store)); the
        // header carries everything the host-membership check and the
        // restore below need, and `builder.open` performs the one full
        // parse. The host filter is fail-closed as before.
        repo.header(requested)
            .await
            .ok()
            .filter(|header| crate::host::belongs_to_current_host(header.metadata.as_ref()))
            .map(|header| (requested.clone(), header))
    } else {
        repo.list().await.ok().and_then(|list| {
            // Only this host's sessions are eligible for restore; an explicit
            // path from another host is not restored (fail-closed).
            let mut list = list
                .into_iter()
                .filter(|info| crate::host::belongs_to_current_host(info.metadata.as_ref()));
            list.find(|info| info.has_messages).map(|info| {
                (
                    info.path.clone(),
                    manox_harness::session::jsonl::JsonlSessionMetadata {
                        id: info.id,
                        cwd: info.cwd,
                        created_at: info.created_at,
                        parent_session_path: info.parent_session_path.map(PathBuf::from),
                        metadata: info.metadata,
                    },
                )
            })
        })
    };
    let mut restored = false;
    let mut session = None;
    if let Some((session_path, header)) = latest {
        // Sessions created by a GUI launch (process cwd `/`) persisted a
        // useless cwd; heal them to this launch's default instead.
        let mut tool_cwd = PathBuf::from(header.cwd.clone());
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
            &state.question_gate,
            &state.plan,
            &notice_tx,
            state.goal_bridge.as_ref(),
            &state.granted_roots,
            &thread_id,
            None,
            &bus,
        );
        match builder.open(session_path).await {
            Ok(mut s) => {
                attach_orchestrators(&mut s, &orchestrators);
                attach_plan_hooks(&mut s, &state.plan, &tool_cwd, read_only_subagent);
                attach_plugin_hooks(&mut s, &tool_cwd);
                attach_prefix_gate(&mut s, &notice_tx, &thread_id);
                adopt_session_model(&s, &mut pi_model, &state);
                restored = true;
                // The restored file is the thread's active session.
                crate::thread_registry::set_active(&thread_id, &header.id).await;
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
                &state.question_gate,
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
                    attach_plan_hooks(&mut s, &state.plan, &cwd, read_only_subagent);
                    attach_plugin_hooks(&mut s, &cwd);
                    attach_prefix_gate(&mut s, &notice_tx, &thread_id);
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
    // A fresh chain announces exactly one `cwd_change` witness for its
    // effective cwd (the projection fold never reads the session header).
    // A restore announces nothing: the reopened chain already carries (or
    // truly lacks) its own moves.
    if !restored {
        announce_established_cwd(&session, &state, &notice_tx).await;
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
    // Seed the cwd display right after `Ready`: a restored session may
    // project an effective cwd (a `cwd_change` tail) that differs from the
    // launch directory, and the facade mirror starts empty. Change-gated —
    // a fresh chain's establishment announcement (above) already reported
    // the value; a restored chain's tail lands exactly once here.
    let projected = session.projected_cwd().await;
    let projected_str = projected.to_string_lossy().into_owned();
    if state.last_cwd_note.lock().unwrap().as_deref() != Some(projected_str.as_str()) {
        *state.last_cwd_note.lock().unwrap() = Some(projected_str.clone());
        let _ = notice_tx.send(BackendNotice::Event(Box::new(ThreadEvent::CwdChanged {
            path: projected_str,
        })));
    }
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
                // steering-queue check inside the helper decides whether a
                // run is owed. A monitor steered events while the session was
                // idle — resume now (S1/DSH parity via the shared helper).
                while wakeup_rx.try_recv().is_ok() {}
                resume_steering_queue(
                    &mut session,
                    &mut cmd_rx,
                    &mut run_steers,
                    &mut shutdown_after_run,
                    Arc::clone(&live_mirror),
                    &state,
                    &notice_tx,
                    &mut pi_model,
                    &sessions_dir,
                    &cwd,
                )
                .await;
                if shutdown_after_run {
                    break;
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
                // #805 companion: the host registers its client tools only
                // after it learns this session's id — i.e. after Open/Create
                // spawned this engine — so re-consult the embedder provider
                // before every prompt (no-op while the registration set is
                // unchanged) instead of trusting the one-time assembly
                // snapshot.
                refresh_embedder_tools(&mut session, &thread_id, &state.gate).await;
                #[cfg(feature = "mcp")]
                refresh_mcp_tools(&mut session, &thread_id, &state.gate).await;
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
                // S2 (pi `agent-loop` / omp settle-drain): a steer enqueued in
                // this run's final moments — after the loop's last turn-boundary
                // drain but before `Settled` — is still sitting in the surviving
                // kernel queue. Drain it here (the helper no-ops when the queue
                // is empty, so a normal prompt turn pays nothing) instead of
                // leaving the user's input stranded to an unrelated later turn.
                resume_steering_queue(
                    &mut session,
                    &mut cmd_rx,
                    &mut run_steers,
                    &mut shutdown_after_run,
                    Arc::clone(&live_mirror),
                    &state,
                    &notice_tx,
                    &mut pi_model,
                    &sessions_dir,
                    &cwd,
                )
                .await;
                if shutdown_after_run {
                    break;
                }
            }
            SessionCmd::Steer { id, text, images } => {
                // S1 (dsh `wakeDriver` idle-steer): a steer that arrives while
                // the actor is idle no longer waits for an unrelated next turn.
                // Enqueue it under its command id (S4 retractable; S3 the same
                // id is the injected `user` row's durable identity), then
                // resume immediately — `continue_` drains the queue on its
                // first poll, injecting the steer and landing the row.
                session
                    .handle()
                    .steer_with_id(steer_message(id.clone(), text, images), id.clone());
                run_steers.push(id);
                resume_steering_queue(
                    &mut session,
                    &mut cmd_rx,
                    &mut run_steers,
                    &mut shutdown_after_run,
                    Arc::clone(&live_mirror),
                    &state,
                    &notice_tx,
                    &mut pi_model,
                    &sessions_dir,
                    &cwd,
                )
                .await;
                if shutdown_after_run {
                    break;
                }
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
            SessionCmd::RequestPlanMode { enabled } => {
                state.plan.set_requested(Some(enabled));
                // The selection is a logged fact from the moment it is made;
                // the committed state still only moves at the boundary.
                let appender = session.journal_appender();
                if let Err(err) = append_typed_resilient(
                    &appender,
                    "plan_mode_request",
                    serde_json::json!({ "enabled": enabled }),
                )
                .await
                {
                    // K4 discipline (same as every other idle append): record
                    // the loss, park it for the settle/idle drains and tell
                    // the facade — the actor stays alive. The mode does NOT
                    // move here, so the log never carries a `plan_mode_change`
                    // without its request.
                    if let Some(row) =
                        record_journal_loss(&appender, "plan_mode_request", &err).await
                    {
                        state.pending_journal.lock().unwrap().push(row);
                    }
                    // The selection goes with the dropped row: committing it
                    // later would put a `plan_mode_change` in the log with no
                    // request behind it. The user got the Error notice, so
                    // re-selecting the mode is the retry.
                    state.plan.set_requested(None);
                    let _ = notice_tx.send(BackendNotice::Event(Box::new(ThreadEvent::Error(
                        anyhow::anyhow!(
                            "journal append permanently failed for `plan_mode_request`: {err:#}; the selection was dropped — switch the mode again to retry"
                        ),
                    ))));
                    continue;
                }
                // Idle threads have no boundary to wait for, so the selection
                // commits immediately (dsh parity — its `set()` appends
                // between turns). Mid-run selections never reach this arm:
                // `drive_run` records them and leaves the commit to the
                // Prompt boundary.
                if !state.running.load(Ordering::Relaxed) {
                    commit_requested_plan_mode(session.path(), &sessions_dir, &state, &notice_tx)
                        .await;
                }
            }
            SessionCmd::SetPlanReviewPending(pending) => {
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
                        write_plan_file_sidecar(&sessions_dir, session.path(), &state.plan).await
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
                    write_plan_file_sidecar(&sessions_dir, session.path(), &state.plan).await
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
                    write_plan_file_sidecar(&sessions_dir, session.path(), &state.plan).await
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
                handle_set_cwd(&mut session, &state, &notice_tx, &path).await;
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
    // release the task center's registry entries. `cleanup_thread` alone
    // only `retain`s (drops entries without cancelling tokens); the cancel
    // must run first or a deleted thread's Sailors become unfindable zombies
    // still burning tokens.
    crate::background_task::cancel_all_for_thread(&thread_id).await;
    crate::background_task::cleanup_thread(&thread_id);
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

/// Render the plan-mode-active instructions (the actor renders them itself,
/// so no facade round-trip is needed on restore or session switches).
fn render_plan_instructions() -> Option<String> {
    let plans_dir = crate::paths::plans_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| ".manox/plans".to_string());
    match crate::collaboration_mode::render_plan_mode_active(&plans_dir) {
        Ok(text) => Some(text),
        Err(err) => {
            tracing::warn!(error = %err, "failed to render plan-mode instructions");
            None
        }
    }
}

/// Commit any outstanding plan-mode selection: the turn-boundary half of the
/// `plan_mode_request` / `plan_mode_change` pair. A selection already equal to
/// the committed state converges silently (it was a no-op intent), so the log
/// carries no redundant transition.
async fn commit_requested_plan_mode(
    session_path: &Path,
    sessions_dir: &Path,
    state: &Arc<EngineState>,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
) {
    let Some(enabled) = state.plan.requested() else {
        return;
    };
    if enabled != state.plan.enabled() {
        apply_plan_mode(session_path, sessions_dir, state, notice_tx, enabled).await;
    } else {
        state.plan.set_requested(None);
    }
}

/// Apply a committed plan-mode switch: the in-memory state, the rendered
/// instructions, the sidecar cache and the `PlanModeChanged` notice (whose
/// tap emission journals the `plan_mode_change` entry, L3).
async fn apply_plan_mode(
    session_path: &Path,
    sessions_dir: &Path,
    state: &Arc<EngineState>,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
    enabled: bool,
) {
    let plan_file = enabled.then(|| state.plan.plan_file()).flatten();
    state.plan.set(enabled, plan_file);
    state
        .plan
        .set_active_instructions(enabled.then(render_plan_instructions).flatten());
    if let Err(err) = write_plan_file_sidecar(sessions_dir, session_path, &state.plan).await {
        tracing::warn!(error = %err, "failed to persist plan mode");
    }
    let _ = notice_tx.send(BackendNotice::Event(Box::new(
        ThreadEvent::PlanModeChanged { enabled },
    )));
}

/// Persist the last plan file from the shared state into the session sidecar
/// (kept across exits for the execution handoff). Plan mode itself is
/// journal-only: the `plan_mode_change` / `plan_mode_request` entries are the
/// single source, so no mode mirror lives here.
async fn write_plan_file_sidecar(
    sessions_dir: &Path,
    session_path: &Path,
    plan: &crate::plan_mode::PlanSessionState,
) -> Result<(), anyhow::Error> {
    manox_harness::session_meta::update(sessions_dir, session_path, |meta| {
        meta.plan_file = plan.plan_file();
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
        plan_mode: replayed.plan_mode.unwrap_or(false),
        plan_file: meta.plan_file.clone(),
        plan_review_pending: replayed.plan_review_pending.unwrap_or(false),
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

/// Journal the fresh session chain's one `cwd_change` witness: the
/// projection fold reads journal entries, never the session file's header,
/// so every establishment (startup build, `NewSession` swap) records its
/// effective cwd unconditionally — the witness count is per chain, not per
/// actor. No value guard: a guard would couple the witness to command
/// arrival order (`SetCwd` advances `last_cwd_note`; a swap-then-set or
/// set-then-swap shuffle could silently swallow the new chain's entry),
/// while duplicate publishes are already idempotent downstream — the fold
/// re-derives the same value and the client merge stays higher-seq-wins.
async fn announce_established_cwd(
    session: &AgentSession,
    state: &Arc<EngineState>,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
) {
    let projected = session.projected_cwd().await;
    let projected_str = projected.to_string_lossy().into_owned();
    let appender = session.journal_appender();
    let payload = serde_json::json!({ "cwd": &projected_str });
    if let Err(err) = append_typed_resilient(&appender, "cwd_change", payload).await {
        if let Some(row) = record_journal_loss(&appender, "cwd_change", &err).await {
            state.pending_journal.lock().unwrap().push(row);
        }
        let _ = notice_tx.send(BackendNotice::Event(Box::new(ThreadEvent::Error(
            anyhow::anyhow!(
                "journal append permanently failed for `cwd_change`: {err:#}; the entry was dropped"
            ),
        ))));
    }
    *state.last_cwd_note.lock().unwrap() = Some(projected_str.clone());
    let _ = notice_tx.send(BackendNotice::Event(Box::new(ThreadEvent::CwdChanged {
        path: projected_str,
    })));
}

/// The host-driven working-directory switch (`SetCwd`): a real move is
/// durable through `set_session_cwd` (one `cwd_change` per move); a switch
/// onto the projected tail only re-reports the facade note — the chain
/// never gains a duplicate entry.
async fn handle_set_cwd(
    session: &mut AgentSession,
    state: &Arc<EngineState>,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
    path: &Path,
) {
    if !path.is_dir() {
        let _ = notice_tx.send(BackendNotice::Event(Box::new(ThreadEvent::Error(
            anyhow::anyhow!(
                "set_cwd: working directory does not exist: {}",
                path.display()
            ),
        ))));
        return;
    }
    if session.projected_cwd().await == path {
        let path_str = path.to_string_lossy().into_owned();
        *state.last_cwd_note.lock().unwrap() = Some(path_str.clone());
        let _ = notice_tx.send(BackendNotice::Event(Box::new(ThreadEvent::CwdChanged {
            path: path_str,
        })));
        return;
    }
    if let Err(err) = session.set_session_cwd(path.to_path_buf()).await {
        let _ = notice_tx.send(BackendNotice::Event(Box::new(ThreadEvent::Error(
            anyhow::anyhow!("set_cwd failed: {err:#}"),
        ))));
        return;
    }
    let path_str = path.to_string_lossy().into_owned();
    *state.last_cwd_note.lock().unwrap() = Some(path_str.clone());
    let _ = notice_tx.send(BackendNotice::Event(Box::new(ThreadEvent::CwdChanged {
        path: path_str,
    })));
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
        superseded_by: None,
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
        // The mirror reads no sidecar, so it cannot know the human-interaction
        // stamp: these two columns carry the last durable write. `ThreadStore`
        // is the sidebar's ordering authority; nothing sorts off this mirror.
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
#[path = "adapt.rs"]
pub mod adapt;
#[cfg(not(feature = "test-support"))]
#[path = "adapt.rs"]
pub(crate) mod adapt;
