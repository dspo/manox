//! Process-global registry of background tasks — command Monitor, WebSocket
//! Monitor, background Bash (proxied from the pi-side registries), and
//! asynchronously-dispatched subagents — each with an owner thread, status,
//! stop routing, and a bounded output ring.
//!
//! This is the host's single source of truth for task cards: the session
//! producers report their lifecycle straight into it through `attach`
//! (the host `TaskObserver`), Sailors register directly, and every state
//! change a path can observe is re-emitted as a
//! `ThreadEvent::BackgroundTaskUpdated` snapshot — producer emissions via
//! the observer, and the stop path's own synchronous terminal push via the
//! per-task notifier the observer registers. Output snapshots are
//! throttled (every fifth line), so "each change" means each lifecycle
//! change plus sampled output.
//!
//! Settlement is first-wins: `push_terminal` transitions a Running/Stopping
//! task, so duplicate terminal reports from a
//! kill path and a natural exit collapse into one terminal state. The
//! terminal status doubles as the cause vocabulary (`Stopped` = explicit
//! TaskStop / user cancel, `SessionEnded` = thread or app teardown,
//! `Completed`/`Failed`/`TimedOut` = the work's own end).
//!
//! Tasks persist after exit so a final poll or status card can observe the
//! terminal state; a periodic GC sweep removes long-dead entries. Task ids
//! issued by the pi-side registries are process-unique (one shared ordinal);
//! directly-registered tasks (subagents) draw from the registry counter.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;

use crate::thread::ThreadEvent;
use crate::thread_engine::BackendNotice;

/// How long a completed task stays in the registry before GC sweeps it.
const GC_AFTER_EXIT: Duration = Duration::from_secs(300);

/// Hard cap on the accumulated event buffer per task (256 KiB).
const MAX_BUFFER_BYTES: usize = 256 * 1024;

/// Maximum events retained in the ring buffer before eviction kicks in.
const MAX_RING_EVENTS: usize = 4096;

/// Unique identifier for a background task.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TaskId(pub String);

impl TaskId {
    pub fn new(prefix: &str, n: u64) -> Self {
        Self(format!("{prefix}_{n}"))
    }
}

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// What kind of background task this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TaskKind {
    MonitorCommand,
    MonitorWebSocket,
    BackgroundBash,
    /// An asynchronously-dispatched subagent coroutine running in a background
    /// pi session. Completion is delivered to the Captain as a
    /// `BackendNotice::SteerDelivered{reason: Complete}` peer message; a
    /// Running snapshot is emitted at dispatch + at settlement so the UI card
    /// surfaces during the run and its Stop button (`background_task::stop`)
    /// cancels the child token, which the run task observes to abort the child
    /// session.
    Subagent,
}

/// The terminal status of a background task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TaskStatus {
    Running,
    Stopping,
    Completed,
    Failed,
    TimedOut,
    Stopped,
    SessionEnded,
}

impl TaskStatus {
    pub fn is_terminal(self) -> bool {
        !matches!(self, TaskStatus::Running | TaskStatus::Stopping)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            TaskStatus::Running => "Running",
            TaskStatus::Stopping => "Stopping",
            TaskStatus::Completed => "Completed",
            TaskStatus::Failed => "Failed",
            TaskStatus::TimedOut => "Timed out",
            TaskStatus::Stopped => "Stopped",
            TaskStatus::SessionEnded => "Session ended",
        }
    }
}

/// What a recorded ring entry represents. The ring only ever stores `Output`
/// lines and the terminal record; UI consumers match on the kind.
#[derive(Debug, Clone)]
pub enum TaskEventKind {
    /// A line of stdout (command) or a text frame (WebSocket).
    Output(String),
    /// The task reached a terminal state.
    Terminal {
        status: TaskStatus,
        exit_code: Option<i32>,
        failure_summary: Option<String>,
    },
}

/// One recorded output line (or terminal record) of a background task, kept
/// in the per-task ring for UI card bodies.
#[derive(Debug, Clone)]
pub struct TaskEvent {
    pub task_id: TaskId,
    pub kind: TaskKind,
    /// Vestigial goal fence: no producer stamps a goal since the goal-fenced
    /// registration APIs were removed; always `None`.
    pub owner_goal_id: Option<String>,
    pub event: TaskEventKind,
    /// Per-task sequence number (ring arrival order).
    pub task_seq: u64,
    pub timestamp_ms: u64,
}

fn now_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ─── TaskState ──────────────────────────────────────────────────────────────

/// Shared state for one background task.
struct TaskState {
    kind: TaskKind,
    owner_thread_id: String,
    description: String,
    status: TaskStatus,
    cancel: CancellationToken,
    /// Monotonic per-task event sequence.
    task_seq: u64,
    /// Ring buffer of recent events (for UI card bodies and the snapshot's
    /// `output_tail` projection).
    events: VecDeque<TaskEvent>,
    events_byte_count: usize,
    total_bytes: u64,
    event_count: u64,
    created_at_ms: u64,
    exited_at: Option<Instant>,
    exited_at_ms: Option<u64>,
    /// The hook that actually stops the underlying pi-side work. Registered
    /// by whoever proxies the task (monitor manager / background manager).
    on_stop: Option<OnStopHook>,
    /// Notice sink for snapshots this registry pushes itself (the stop
    /// path's synchronous terminal status). Producer-driven changes emit
    /// through the observer instead; tasks registered without an observer
    /// (Sailors) deliver their own notices.
    notifier: Option<mpsc::UnboundedSender<BackendNotice>>,
    exit_code: Option<i32>,
    /// Truncated failure/stderr summary.
    failure_summary: Option<String>,
}

impl TaskState {
    fn new(
        kind: TaskKind,
        owner_thread_id: String,
        description: String,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            kind,
            owner_thread_id,
            description,
            status: TaskStatus::Running,
            cancel,
            task_seq: 0,
            events: VecDeque::new(),
            events_byte_count: 0,
            total_bytes: 0,
            event_count: 0,
            created_at_ms: now_ts(),
            exited_at: None,
            exited_at_ms: None,
            on_stop: None,
            notifier: None,
            exit_code: None,
            failure_summary: None,
        }
    }
}

/// The hook that actually stops a proxy's underlying work (the pi-side
/// kill). The second argument carries the stopping side's intent so the
/// producer's kill site records the right [`SettlementCause`] — a host
/// teardown (`SessionEnded`) forwards `Teardown`, a user-facing stop
/// (`Stopped`) forwards `UserStop`.
pub type OnStopHook = Arc<dyn Fn(&str, manox_harness::tasks::SettlementCause) + Send + Sync>;

/// A registered background task.
pub struct BackgroundTask {
    state: Arc<std::sync::Mutex<TaskState>>,
    completion: Arc<Notify>,
}

impl BackgroundTask {
    fn new(
        kind: TaskKind,
        owner_thread_id: String,
        description: String,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            state: Arc::new(std::sync::Mutex::new(TaskState::new(
                kind,
                owner_thread_id,
                description,
                cancel,
            ))),
            completion: Arc::new(Notify::new()),
        }
    }

    pub fn status(&self) -> TaskStatus {
        self.state.lock().expect("task state poisoned").status
    }

    pub fn is_running(&self) -> bool {
        matches!(
            self.state.lock().expect("task state poisoned").status,
            TaskStatus::Running | TaskStatus::Stopping
        )
    }

    pub fn cancel(&self) {
        self.state
            .lock()
            .expect("task state poisoned")
            .cancel
            .cancel();
    }

    /// Push an output line into the task's bounded ring.
    pub fn push_event(&self, task_id: &TaskId, text: String) {
        let mut s = self.state.lock().expect("task state poisoned");
        s.task_seq += 1;
        let event = TaskEvent {
            task_id: task_id.clone(),
            kind: s.kind,
            owner_goal_id: None,
            event: TaskEventKind::Output(text.clone()),
            task_seq: s.task_seq,
            timestamp_ms: now_ts(),
        };
        s.event_count += 1;
        s.total_bytes = s.total_bytes.saturating_add(text.len() as u64);
        s.events.push_back(event);
        s.events_byte_count += text.len();
        while s.events_byte_count > MAX_BUFFER_BYTES || s.events.len() > MAX_RING_EVENTS {
            match s.events.pop_front() {
                Some(removed) => {
                    if let TaskEventKind::Output(t) = &removed.event {
                        s.events_byte_count = s.events_byte_count.saturating_sub(t.len());
                    }
                }
                None => break,
            }
        }
    }

    /// Push a terminal event. First-wins: only Running/Stopping can
    /// transition. Returns whether THIS call performed the transition —
    /// callers that emit notices use it to skip duplicates (a kill-site
    /// settlement and the stop path's fallback share one channel, so the
    /// loser emitting again would double every journal line).
    pub fn push_terminal(&self, task_id: &TaskId, status: TaskStatus) -> bool {
        let mut s = self.state.lock().expect("task state poisoned");
        if s.status.is_terminal() {
            return false;
        }
        s.status = status;
        s.exited_at = Some(Instant::now());
        s.exited_at_ms = Some(now_ts());
        s.task_seq += 1;
        let event = TaskEvent {
            task_id: task_id.clone(),
            kind: s.kind,
            owner_goal_id: None,
            event: TaskEventKind::Terminal {
                status,
                exit_code: s.exit_code,
                failure_summary: s.failure_summary.clone(),
            },
            task_seq: s.task_seq,
            timestamp_ms: now_ts(),
        };
        s.events.push_back(event);
        drop(s);
        self.completion.notify_waiters();
        true
    }

    /// Set the exit code (for command/Bash tasks).
    pub fn set_exit_code(&self, code: Option<i32>) {
        self.state.lock().expect("task state poisoned").exit_code = code;
    }

    /// Set a truncated failure summary (stderr or error message).
    pub fn set_failure_summary(&self, summary: String) {
        let truncated = if summary.len() > 2048 {
            let boundary = summary.floor_char_boundary(2048);
            format!("{}…", &summary[..boundary])
        } else {
            summary
        };
        self.state
            .lock()
            .expect("task state poisoned")
            .failure_summary = Some(truncated);
    }

    /// Atomically become the lifecycle owner for a stop. Concurrent callers
    /// wait for this owner instead of returning before the work stops.
    fn begin_stopping(&self) -> bool {
        let mut s = self.state.lock().expect("task state poisoned");
        match s.status {
            TaskStatus::Running => {
                s.status = TaskStatus::Stopping;
                true
            }
            TaskStatus::Stopping
            | TaskStatus::Completed
            | TaskStatus::Failed
            | TaskStatus::TimedOut
            | TaskStatus::Stopped
            | TaskStatus::SessionEnded => false,
        }
    }

    async fn wait_until_terminal(&self) {
        loop {
            let notified = self.completion.notified();
            if self.status().is_terminal() {
                return;
            }
            notified.await;
        }
    }

    /// Get recent events from the ring buffer (for UI card bodies).
    pub fn recent_events(&self) -> Vec<TaskEvent> {
        self.state
            .lock()
            .expect("task state poisoned")
            .events
            .iter()
            .cloned()
            .collect()
    }

    /// The registered stop hook, if any.
    pub fn on_stop(&self) -> Option<OnStopHook> {
        self.state
            .lock()
            .expect("task state poisoned")
            .on_stop
            .clone()
    }

    /// Register the hook that actually stops the underlying pi-side work when
    /// this task's stop path runs. Called once by the owner that created the
    /// proxy.
    pub fn set_on_stop(&self, on_stop: OnStopHook) {
        self.state.lock().expect("task state poisoned").on_stop = Some(on_stop);
    }

    /// Register the notice sink for snapshots this registry pushes itself
    /// (the stop path's synchronous terminal status). Called by the host
    /// observer at proxy registration.
    pub fn set_notifier(&self, notifier: mpsc::UnboundedSender<BackendNotice>) {
        self.state.lock().expect("task state poisoned").notifier = Some(notifier);
    }

    /// Emit a snapshot through the registered notifier, if any. The
    /// snapshot is built and sent under the state lock: a read-then-send
    /// window would let a sampled `Running` snapshot land after a terminal
    /// one and flip the card backwards.
    fn emit_snapshot(&self, task_id: &TaskId) {
        let s = self.state.lock().expect("task state poisoned");
        let snapshot = build_snapshot(&s, task_id);
        if let Some(tx) = s.notifier.as_ref() {
            let _ = tx.send(BackendNotice::Event(Box::new(
                ThreadEvent::BackgroundTaskUpdated { snapshot },
            )));
        }
    }

    pub fn owner_thread_id(&self) -> String {
        self.state
            .lock()
            .expect("task state poisoned")
            .owner_thread_id
            .clone()
    }

    /// Build a serializable snapshot for UI cards.
    pub fn snapshot(&self, task_id: &TaskId) -> TaskSnapshot {
        let s = self.state.lock().expect("task state poisoned");
        build_snapshot(&s, task_id)
    }
}

/// Build a snapshot from an already-locked state (shared by the UI
/// projection and the notifier emission, which must read and send under
/// one lock hold).
fn build_snapshot(s: &TaskState, task_id: &TaskId) -> TaskSnapshot {
    TaskSnapshot {
        task_id: task_id.0.clone(),
        kind: s.kind,
        owner_thread_id: s.owner_thread_id.clone(),
        description: s.description.clone(),
        status: s.status,
        created_at_ms: s.created_at_ms,
        ended_at_ms: s.exited_at_ms,
        event_count: s.event_count,
        total_bytes: s.total_bytes,
        exit_code: s.exit_code,
        failure_summary: s.failure_summary.clone(),
        anchor_message_id: None,
        output_tail: output_tail_from_ring(s),
    }
}

/// A serializable snapshot of a background task's state. This shape is on the
/// journal and the AHP wire (`x-manox-work/backgroundTasksChanged`): field
/// names and status strings are wire-stable.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TaskSnapshot {
    pub task_id: String,
    pub kind: TaskKind,
    pub owner_thread_id: String,
    pub description: String,
    pub status: TaskStatus,
    pub created_at_ms: u64,
    pub ended_at_ms: Option<u64>,
    pub event_count: u64,
    pub total_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor_message_id: Option<String>,
    /// Bounded tail of accumulated output (newest bytes), for wire projection
    /// to UI cards without a registry round-trip.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub output_tail: String,
}

/// Cap on the `output_tail` bytes carried in a snapshot.
const SNAPSHOT_TAIL_BYTES: usize = 8 * 1024;

/// Rebuild a bounded output tail from the task's event ring.
fn output_tail_from_ring(s: &TaskState) -> String {
    let mut tail = String::new();
    for ev in &s.events {
        if let TaskEventKind::Output(text) = &ev.event {
            if !tail.is_empty() {
                tail.push('\n');
            }
            // Lines usually arrive with their own trailing newline; strip it
            // so the joined tail has no blank lines.
            tail.push_str(text.trim_end_matches(['\r', '\n']));
        }
    }
    if tail.len() > SNAPSHOT_TAIL_BYTES {
        // ceil_char_boundary keeps the cut on a UTF-8 char boundary within
        // the cap; a raw byte split can land mid-character and panic.
        tail.split_off(tail.ceil_char_boundary(tail.len() - SNAPSHOT_TAIL_BYTES))
    } else {
        tail
    }
}

impl TaskSnapshot {
    /// A persisted Running/Stopping task cannot still be attached to a process
    /// after a new app session starts. Preserve the record, but make the stale
    /// lifecycle boundary explicit instead of showing a forever-running card.
    pub fn normalize_after_restore(mut self) -> Self {
        if matches!(self.status, TaskStatus::Running | TaskStatus::Stopping) {
            self.status = TaskStatus::SessionEnded;
            self.ended_at_ms = Some(now_ts());
            if self.failure_summary.is_none() {
                self.failure_summary = Some("The previous manox session ended.".into());
            }
        }
        self
    }
}

// ─── Registry ───────────────────────────────────────────────────────────────

/// The process-global background task registry.
struct Registry {
    tasks: HashMap<String, Arc<BackgroundTask>>,
    /// Ordinal for directly-registered tasks (subagents).
    next_id: u64,
}

static REGISTRY: OnceLock<std::sync::Mutex<Registry>> = OnceLock::new();

fn registry() -> &'static std::sync::Mutex<Registry> {
    REGISTRY.get_or_init(|| {
        std::sync::Mutex::new(Registry {
            tasks: HashMap::new(),
            next_id: 1,
        })
    })
}

/// Allocate a unique id for a directly-registered task. Direct registration
/// is subagent-only (pi-path tasks enter through `register_with_id` under
/// their harness-issued ids), so the prefix namespace never overlaps the
/// harness registries' `mon_`/`bg_`/`ws_`.
fn next_id() -> TaskId {
    let mut reg = registry().lock().expect("registry poisoned");
    let n = reg.next_id;
    reg.next_id += 1;
    TaskId::new("subagent", n)
}

/// Register an asynchronously-dispatched subagent and return its id and
/// handle. Subagents are the only directly-registered tasks; every pi-path
/// producer enters through `register_with_id` instead.
pub fn register(
    owner_thread_id: String,
    description: String,
    cancel: CancellationToken,
) -> (TaskId, Arc<BackgroundTask>) {
    gc();
    let id = next_id();
    let task = Arc::new(BackgroundTask::new(
        TaskKind::Subagent,
        owner_thread_id,
        description,
        cancel,
    ));
    let mut reg = registry().lock().expect("registry poisoned");
    reg.tasks.insert(id.0.clone(), task.clone());
    drop(reg);
    (id, task)
}

/// Register a proxy task under a caller-chosen id (the pi-side task id the
/// bridge mirrors). Used so `stop` sees the same id as the underlying pi task.
/// Idempotent: an existing entry with the id is returned untouched. Pi-side
/// ids are process-unique (one shared ordinal), so no cross-task collision
/// can reach this call.
pub fn register_with_id(
    id: TaskId,
    kind: TaskKind,
    owner_thread_id: String,
    description: String,
    cancel: CancellationToken,
) -> Arc<BackgroundTask> {
    gc();
    let mut reg = registry().lock().expect("registry poisoned");
    if let Some(existing) = reg.tasks.get(&id.0) {
        return Arc::clone(existing);
    }
    let task = Arc::new(BackgroundTask::new(
        kind,
        owner_thread_id,
        description,
        cancel,
    ));
    reg.tasks.insert(id.0.clone(), Arc::clone(&task));
    drop(reg);
    task
}

/// Look up a task by string id.
pub fn get_by_str(id: &str) -> Option<Arc<BackgroundTask>> {
    registry()
        .lock()
        .expect("registry poisoned")
        .tasks
        .get(id)
        .cloned()
}

/// Remove a task from the registry.
pub fn remove(id: &TaskId) {
    registry()
        .lock()
        .expect("registry poisoned")
        .tasks
        .remove(&id.0);
}

/// Record a harness settlement on a task: the one mapping from the unified
/// lifecycle vocabulary to the wire-stable terminal status. First-wins like
/// `push_terminal`; cause `Teardown` settles as `SessionEnded`.
pub(crate) fn apply_settlement(
    task: &BackgroundTask,
    id: &TaskId,
    settlement: &manox_harness::tasks::Settlement,
) {
    use manox_harness::tasks::{SettlementCause, SettlementKind};
    if let Some(code) = settlement.exit_code {
        task.set_exit_code(Some(code));
    }
    if let Some(reason) = &settlement.failure_summary {
        task.set_failure_summary(reason.clone());
    }
    let status = match settlement.kind {
        SettlementKind::Completed => TaskStatus::Completed,
        SettlementKind::Failed => TaskStatus::Failed,
        SettlementKind::TimedOut => TaskStatus::TimedOut,
        SettlementKind::Stopped => {
            if settlement.cause == SettlementCause::Teardown {
                TaskStatus::SessionEnded
            } else {
                TaskStatus::Stopped
            }
        }
    };
    task.push_terminal(id, status);
}

/// Output lines collected before a task's snapshot re-emits to the UI.
const OUTPUT_EMIT_THRESHOLD: u32 = 5;

/// The host's [`TaskObserver`]: binds the session producers (monitors,
/// background bash) to this registry. Each `Spawned` registers a proxy task
/// under the pi task id (one id space for `stop` and the UI cards, with an
/// on_stop hook back into the producer), each `Output` lands in the proxy's
/// bounded ring (snapshots throttled), and each `Settled` records the
/// wire-stable terminal status through `apply_settlement`.
struct HostTaskObserver {
    notice_tx: mpsc::UnboundedSender<BackendNotice>,
    owner_thread_id: String,
    output_since_emit: Mutex<HashMap<String, u32>>,
}

impl manox_harness::tasks::TaskObserver for HostTaskObserver {
    fn on_spawned(
        &self,
        id: &str,
        family: manox_harness::tasks::TaskFamily,
        label: &str,
        stop: manox_harness::tasks::StopHandle,
    ) {
        let proxy = register_with_id(
            TaskId(id.to_string()),
            map_kind(family),
            self.owner_thread_id.clone(),
            label.to_string(),
            CancellationToken::new(),
        );
        proxy.set_on_stop(Arc::new(move |_, cause| stop(cause)));
        proxy.set_notifier(self.notice_tx.clone());
        self.emit_snapshot(&proxy, id);
    }

    fn on_output(&self, id: &str, line: String) {
        let Some(proxy) = get_by_str(id) else {
            tracing::warn!(target: "tasks", id, "output for unregistered task; dropping line");
            return;
        };
        proxy.push_event(&TaskId(id.to_string()), line);
        let emit = {
            let mut counts = self
                .output_since_emit
                .lock()
                .expect("output counter poisoned");
            let count = counts.entry(id.to_string()).or_insert(0);
            *count += 1;
            if *count >= OUTPUT_EMIT_THRESHOLD {
                *count = 0;
                true
            } else {
                false
            }
        };
        if emit {
            self.emit_snapshot(&proxy, id);
        }
    }

    fn on_settled(&self, id: &str, settlement: &manox_harness::tasks::Settlement) {
        let Some(proxy) = get_by_str(id) else {
            tracing::warn!(target: "tasks", id, "settled task has no host proxy; dropping settlement");
            return;
        };
        apply_settlement(&proxy, &TaskId(id.to_string()), settlement);
        self.emit_snapshot(&proxy, id);
    }
}

impl HostTaskObserver {
    /// Emit a `BackgroundTaskUpdated` notice for a task. Built and sent
    /// under the task's state lock (see `BackgroundTask::emit_snapshot`):
    /// the notifier registered here is the same channel, so both paths
    /// share one stale-window-free primitive.
    fn emit_snapshot(&self, task: &BackgroundTask, id: &str) {
        task.emit_snapshot(&TaskId(id.to_string()));
    }
}

fn map_kind(family: manox_harness::tasks::TaskFamily) -> TaskKind {
    match family {
        manox_harness::tasks::TaskFamily::MonitorCommand => TaskKind::MonitorCommand,
        manox_harness::tasks::TaskFamily::MonitorWebSocket => TaskKind::MonitorWebSocket,
        manox_harness::tasks::TaskFamily::BackgroundBash => TaskKind::BackgroundBash,
        manox_harness::tasks::TaskFamily::Subagent => TaskKind::Subagent,
    }
}

/// Bind the session's producers to the host observer. Called next to
/// `attach_orchestrators` at session build/restore; the observer lives as
/// long as the managers hold it.
pub fn attach(
    monitor: Arc<manox_harness::monitor::MonitorManager>,
    background: Arc<manox_harness::bash::orchestration::BackgroundManager>,
    notice_tx: mpsc::UnboundedSender<BackendNotice>,
    owner_thread_id: String,
) {
    let observer = Arc::new(HostTaskObserver {
        notice_tx,
        owner_thread_id,
        output_since_emit: Mutex::new(HashMap::new()),
    });
    monitor.set_observer(Arc::clone(&observer) as Arc<dyn manox_harness::tasks::TaskObserver>);
    background.set_observer(observer);
}

/// Stop a task by id and return only after its stop hook has run and the task
/// has settled. This is the semantic boundary used by TaskStop and shutdown.
pub async fn stop(id: &str) -> Result<(), String> {
    stop_with_status(id, TaskStatus::Stopped).await
}

async fn stop_with_status(id: &str, terminal: TaskStatus) -> Result<(), String> {
    // Get the task without holding the registry lock during stop operations.
    let task = {
        let reg = registry().lock().expect("registry poisoned");
        reg.tasks
            .get(id)
            .cloned()
            .ok_or_else(|| format!("Unknown task id: {id}. {}", list_stoppable_under_lock(&reg)))
    }?;

    // Idempotent: if already terminal, return Ok.
    if task.status().is_terminal() {
        return Ok(());
    }

    if !task.begin_stopping() {
        task.wait_until_terminal().await;
        return Ok(());
    }
    task.cancel();

    // Proxy tasks own no process here; the registered hook is the actual
    // pi-side kill, forwarded with the stop's intent. It runs before the
    // fallback terminal push below.
    if let Some(on_stop) = task.on_stop() {
        let cause = if terminal == TaskStatus::SessionEnded {
            manox_harness::tasks::SettlementCause::Teardown
        } else {
            manox_harness::tasks::SettlementCause::UserStop
        };
        on_stop(id, cause);
    }

    // The pi-side producers normally settle the task themselves through their
    // own settlement path; this fallback covers hooks that cannot report.
    // Either way the terminal status must reach the cards: the notifier (a
    // host-side copy of the observer's notice channel) emits the snapshot
    // when this push is the one that transitioned the task — a producer
    // settlement has already emitted through the same channel, and the
    // fallback would only duplicate it (one journal line per copy).
    if task.push_terminal(&TaskId(id.to_string()), terminal) {
        task.emit_snapshot(&TaskId(id.to_string()));
    }

    Ok(())
}

fn list_stoppable_under_lock(reg: &Registry) -> String {
    let mut lines: Vec<String> = Vec::new();
    for (id, task) in &reg.tasks {
        let s = task.state.lock().expect("task state poisoned");
        if !s.status.is_terminal() {
            let kind_str = match s.kind {
                TaskKind::MonitorCommand => "monitor (command)",
                TaskKind::MonitorWebSocket => "monitor (WebSocket)",
                TaskKind::BackgroundBash => "background bash",
                TaskKind::Subagent => "Subagent",
            };
            lines.push(format!(
                "  {id} — {kind_str} — \"{desc}\" (events: {n})",
                desc = s.description,
                n = s.event_count,
            ));
        }
    }
    if lines.is_empty() {
        "No background tasks are currently running.".into()
    } else {
        format!("Running background tasks:\n{}", lines.join("\n"))
    }
}

/// Run a garbage-collection pass: remove tasks that exited more than
/// `GC_AFTER_EXIT` ago.
pub fn gc() {
    let mut reg = registry().lock().expect("registry poisoned");
    let now = Instant::now();
    reg.tasks.retain(|_, task| {
        let s = task.state.lock().expect("task state poisoned");
        match s.exited_at {
            Some(t) => now.duration_since(t) < GC_AFTER_EXIT,
            None => true,
        }
    });
}

/// Whether a thread has any running (non-terminal) tasks.
pub fn thread_has_running_tasks(thread_id: &str) -> bool {
    let reg = registry().lock().expect("registry poisoned");
    reg.tasks.values().any(|task| {
        let s = task.state.lock().expect("task state poisoned");
        s.owner_thread_id == thread_id && !s.status.is_terminal()
    })
}

/// Cancel all tasks owned by a thread and mark them as SessionEnded.
pub async fn cancel_all_for_thread(thread_id: &str) {
    let ids: Vec<String> = {
        let reg = registry().lock().expect("registry poisoned");
        reg.tasks
            .iter()
            .filter(|(_, task)| task.owner_thread_id() == thread_id && !task.status().is_terminal())
            .map(|(id, _)| id.clone())
            .collect()
    };
    futures::future::join_all(
        ids.iter()
            .map(|id| stop_with_status(id, TaskStatus::SessionEnded)),
    )
    .await;
}

/// Stop every non-terminal task owned by a thread with TaskStop semantics
/// (terminal status `Stopped`). This is the explicit user-cancel counterpart
/// of `cancel_all_for_thread` (thread-teardown: `SessionEnded`); a natural
/// turn settle never calls it — background tasks survive across turns.
pub async fn stop_all_for_thread(thread_id: &str) {
    let ids: Vec<String> = {
        let reg = registry().lock().expect("registry poisoned");
        reg.tasks
            .iter()
            .filter(|(_, task)| task.owner_thread_id() == thread_id && !task.status().is_terminal())
            .map(|(id, _)| id.clone())
            .collect()
    };
    futures::future::join_all(
        ids.iter()
            .map(|id| stop_with_status(id, TaskStatus::Stopped)),
    )
    .await;
}

/// Shutdown all running tasks across all threads. Called at app exit by the
/// host application.
pub async fn shutdown_all() {
    let ids: Vec<String> = {
        let reg = registry().lock().expect("registry poisoned");
        reg.tasks
            .iter()
            .filter(|(_, task)| !task.status().is_terminal())
            .map(|(id, _)| id.clone())
            .collect()
    };
    futures::future::join_all(
        ids.iter()
            .map(|id| stop_with_status(id, TaskStatus::SessionEnded)),
    )
    .await;
}

/// Remove all tasks owned by a thread.
fn remove_all_for_thread(thread_id: &str) {
    let mut reg = registry().lock().expect("registry poisoned");
    reg.tasks
        .retain(|_, task| task.owner_thread_id() != thread_id);
}

/// Release all process-global state owned by a thread: its task registry
/// entries. Called when the thread's engine actor exits.
pub fn cleanup_thread(thread_id: &str) {
    remove_all_for_thread(thread_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire-stable golden serialization of a snapshot: field names and
    /// status strings are consumed by the journal and the AHP wire
    /// (`x-manox-work/backgroundTasksChanged`); any drift here is a
    /// cross-repo breaking change and must be annotated as such.
    #[test]
    fn task_snapshot_serialization_is_wire_stable() {
        let snapshot = TaskSnapshot {
            task_id: "mon_7".into(),
            kind: TaskKind::MonitorCommand,
            owner_thread_id: "t-golden".into(),
            description: "watch the build".into(),
            status: TaskStatus::Completed,
            created_at_ms: 1_000,
            ended_at_ms: Some(2_000),
            event_count: 42,
            total_bytes: 4_096,
            exit_code: Some(0),
            failure_summary: Some("nope".into()),
            anchor_message_id: Some("m1".into()),
            output_tail: "last line".into(),
        };
        let json = serde_json::to_string(&snapshot).unwrap();
        assert_eq!(
            json,
            "{\"task_id\":\"mon_7\",\"kind\":\"MonitorCommand\",\
             \"owner_thread_id\":\"t-golden\",\"description\":\"watch the build\",\
             \"status\":\"Completed\",\"created_at_ms\":1000,\"ended_at_ms\":2000,\
             \"event_count\":42,\"total_bytes\":4096,\"exit_code\":0,\
             \"failure_summary\":\"nope\",\"anchor_message_id\":\"m1\",\
             \"output_tail\":\"last line\"}"
        );
        // Empty/None optionals stay omitted, matching the historic shape.
        let sparse = TaskSnapshot {
            anchor_message_id: None,
            failure_summary: None,
            output_tail: String::new(),
            ..snapshot
        };
        let json = serde_json::to_string(&sparse).unwrap();
        assert!(
            !json.contains("anchor_message_id")
                && !json.contains("failure_summary")
                && !json.contains("output_tail"),
            "optionals must stay omitted: {json}"
        );
    }

    /// The settlement mapping: cause `Teardown` settles as `SessionEnded`
    /// (the card belongs to a session going away); a user stop stays
    /// `Stopped`; a natural completion stays `Completed`.
    #[test]
    fn settlement_cause_maps_to_wire_status() {
        use manox_harness::tasks::{Settlement, SettlementCause, SettlementKind};
        let id = TaskId("map_1".into());
        let teardown = register_with_id(
            id.clone(),
            TaskKind::MonitorCommand,
            "t-map".into(),
            "map".into(),
            CancellationToken::new(),
        );
        apply_settlement(
            &teardown,
            &id,
            &Settlement::new(SettlementKind::Stopped, SettlementCause::Teardown),
        );
        assert_eq!(teardown.status(), TaskStatus::SessionEnded);
        remove(&id);

        let id = TaskId("map_2".into());
        let user_stop = register_with_id(
            id.clone(),
            TaskKind::MonitorWebSocket,
            "t-map".into(),
            "map".into(),
            CancellationToken::new(),
        );
        apply_settlement(
            &user_stop,
            &id,
            &Settlement::new(SettlementKind::Stopped, SettlementCause::UserStop),
        );
        assert_eq!(user_stop.status(), TaskStatus::Stopped);
        remove(&id);
    }

    /// The host observer bridges a real command monitor end-to-end: the
    /// proxy registers under the pi task id, snapshots flow as
    /// `BackgroundTaskUpdated` notices (running → completed with output),
    /// and a user stop routes through the on_stop hook into the monitor
    /// manager.
    #[tokio::test]
    async fn host_observer_bridges_monitor_snapshots() {
        use std::path::PathBuf;

        let monitor = manox_harness::monitor::MonitorManager::new(Arc::new(
            manox_harness::BackgroundRegistry::new(),
        ));
        let background = manox_harness::bash::orchestration::BackgroundManager::new(Arc::new(
            manox_harness::BackgroundRegistry::new(),
        ));
        let (notice_tx, mut notice_rx) = mpsc::unbounded_channel::<BackendNotice>();
        attach(
            Arc::clone(&monitor),
            Arc::clone(&background),
            notice_tx,
            "t1".into(),
        );

        let tid = monitor
            .spawn_command(
                "bridge watcher".into(),
                "echo bridge-line".into(),
                &PathBuf::from("/tmp"),
                Duration::from_secs(30),
                false,
            )
            .unwrap();

        let mut saw_running = false;
        let mut saw_completed = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while tokio::time::Instant::now() < deadline {
            let Ok(Some(notice)) =
                tokio::time::timeout(Duration::from_millis(500), notice_rx.recv()).await
            else {
                break;
            };
            let BackendNotice::Event(ev) = notice else {
                continue;
            };
            let ThreadEvent::BackgroundTaskUpdated { snapshot } = *ev else {
                continue;
            };
            assert_eq!(snapshot.task_id, tid);
            assert_eq!(snapshot.owner_thread_id, "t1");
            match snapshot.status {
                TaskStatus::Running => saw_running = true,
                TaskStatus::Completed => {
                    assert!(snapshot.output_tail.contains("bridge-line"));
                    saw_completed = true;
                    break;
                }
                _ => {}
            }
        }
        assert!(saw_running, "initial running snapshot observed");
        assert!(saw_completed, "terminal completed snapshot observed");
        remove(&TaskId(tid.clone()));

        // A user stop routes through the proxy's on_stop hook into the
        // monitor manager, and the stop path's settlement records Stopped.
        // (A long-running monitor: the echo task above is already terminal.)
        let tid = monitor
            .spawn_command(
                "long watcher".into(),
                "sleep 30".into(),
                &std::path::PathBuf::from("/tmp"),
                Duration::from_secs(60),
                false,
            )
            .unwrap();
        let proxy = get_by_str(&tid).expect("proxy registered");
        assert_eq!(proxy.owner_thread_id(), "t1");
        stop(&tid).await.ok();
        assert_eq!(proxy.status(), TaskStatus::Stopped);
        remove(&TaskId(tid.clone()));
    }

    /// The stop path's own terminal push must reach the cards: stopping a
    /// proxy emits a terminal snapshot through the notifier even if the
    /// producer side (whose observer owns the other notice path) is gone.
    /// This is the host half of the "card stuck on Running" guard.
    #[tokio::test]
    async fn host_stop_emits_terminal_snapshot() {
        let id = TaskId("notified_stop".into());
        let proxy = register_with_id(
            id.clone(),
            TaskKind::MonitorCommand,
            "t-notify".into(),
            "watch".into(),
            CancellationToken::new(),
        );
        let (notice_tx, mut notice_rx) = mpsc::unbounded_channel::<BackendNotice>();
        proxy.set_notifier(notice_tx);

        stop(&id.0).await.expect("stop succeeds");
        assert_eq!(proxy.status(), TaskStatus::Stopped);

        let mut saw_terminal = false;
        for _ in 0..50 {
            match notice_rx.try_recv() {
                Ok(BackendNotice::Event(ev)) => {
                    if let ThreadEvent::BackgroundTaskUpdated { snapshot } = *ev
                        && snapshot.task_id == id.0
                        && snapshot.status == TaskStatus::Stopped
                    {
                        saw_terminal = true;
                        break;
                    }
                }
                Ok(_) => {}
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(20)),
            }
        }
        assert!(
            saw_terminal,
            "the host stop path must emit a terminal snapshot"
        );
        remove(&id);
    }

    /// The stop path must not duplicate a producer's terminal snapshot:
    /// kill-point settlements emit through the same channel the notifier
    /// writes to, so a stop arriving after such a settlement pushes no
    /// second terminal snapshot (one journal line per stop, not two).
    #[tokio::test]
    async fn host_stop_after_producer_settlement_emits_no_duplicate() {
        let id = TaskId("dup_stop".into());
        let proxy = register_with_id(
            id.clone(),
            TaskKind::MonitorCommand,
            "t-dup".into(),
            "watch".into(),
            CancellationToken::new(),
        );
        let (notice_tx, mut notice_rx) = mpsc::unbounded_channel::<BackendNotice>();
        proxy.set_notifier(notice_tx);

        // Producer-side settlement, exactly as the observer's kill-point
        // path does it: apply the settlement, then emit through the locked
        // primitive on the shared channel.
        apply_settlement(
            &proxy,
            &id,
            &manox_harness::tasks::Settlement::new(
                manox_harness::tasks::SettlementKind::Stopped,
                manox_harness::tasks::SettlementCause::UserStop,
            ),
        );
        proxy.emit_snapshot(&id);
        let terminal_notices = |rx: &mut mpsc::UnboundedReceiver<BackendNotice>| {
            let mut n = 0;
            while let Ok(BackendNotice::Event(ev)) = rx.try_recv() {
                if let ThreadEvent::BackgroundTaskUpdated { snapshot } = *ev
                    && snapshot.task_id == id.0
                    && snapshot.status == TaskStatus::Stopped
                {
                    n += 1;
                }
            }
            n
        };
        assert_eq!(terminal_notices(&mut notice_rx), 1, "producer emitted once");

        // The late host stop finds the task already terminal: no second
        // terminal snapshot may be emitted.
        stop(&id.0).await.expect("stop succeeds");
        assert_eq!(
            terminal_notices(&mut notice_rx),
            0,
            "the stop fallback must not duplicate the producer's snapshot"
        );
        remove(&id);
    }

    #[test]
    fn register_and_get() {
        let cancel = CancellationToken::new();
        let (id, task) = register("thread-register".into(), "watch build".into(), cancel);
        assert!(id.0.starts_with("subagent_"));
        assert_eq!(task.status(), TaskStatus::Running);
        assert_eq!(task.snapshot(&id).event_count, 0);
        let found = get_by_str(&id.0).expect("should find task");
        assert_eq!(found.status(), TaskStatus::Running);
        remove(&id);
    }

    #[tokio::test]
    async fn stop_and_terminal() {
        let cancel = CancellationToken::new();
        let (id, task) = register("thread-1".into(), "test".into(), cancel);
        assert_eq!(task.status(), TaskStatus::Running);
        stop(&id.0).await.expect("stop should succeed");
        assert!(task.status().is_terminal());
        stop(&id.0).await.expect("double stop should succeed");
        remove(&id);
    }

    #[tokio::test]
    async fn stop_unknown_task_returns_error_with_list() {
        let result = stop("nonexistent_id").await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("Unknown task id"));
        assert!(err.contains("background tasks") || err.contains("No background"));
    }

    #[test]
    fn push_event_updates_count() {
        let cancel = CancellationToken::new();
        let (id, task) = register("thread-1".into(), "test".into(), cancel);
        task.push_event(&id, "hello".into());
        let snap = task.snapshot(&id);
        assert_eq!(snap.event_count, 1);
        assert_eq!(snap.total_bytes, 5);
        remove(&id);
    }

    #[test]
    fn oversized_single_event_does_not_break_task_ring_byte_cap() {
        let cancel = CancellationToken::new();
        let (id, task) = register("thread-oversized".into(), "oversized event".into(), cancel);
        task.push_event(&id, "x".repeat(MAX_BUFFER_BYTES + 1));
        let snap = task.snapshot(&id);
        assert!(snap.total_bytes >= MAX_BUFFER_BYTES as u64);
        assert!(snap.output_tail.is_empty(), "oversized line evicted whole");
        remove(&id);
    }

    #[test]
    fn push_terminal_is_idempotent() {
        let cancel = CancellationToken::new();
        let (id, task) = register("thread-1".into(), "test".into(), cancel);
        task.push_terminal(&id, TaskStatus::Completed);
        assert_eq!(task.status(), TaskStatus::Completed);
        task.push_terminal(&id, TaskStatus::Failed);
        assert_eq!(task.status(), TaskStatus::Completed);
    }

    #[test]
    fn ring_buffer_evicts_oldest_not_all() {
        let cancel = CancellationToken::new();
        let (id, task) = register("thread-1".into(), "test".into(), cancel);
        let big = "X".repeat(200 * 1024);
        task.push_event(&id, big.clone());
        task.push_event(&id, big.clone());
        task.push_event(&id, "last".into());
        let texts: Vec<String> = task
            .recent_events()
            .into_iter()
            .filter_map(|e| match e.event {
                TaskEventKind::Output(t) => Some(t),
                _ => None,
            })
            .collect();
        assert!(
            texts.contains(&"last".to_string()),
            "last event should survive, got: {texts:?}"
        );
        remove(&id);
    }

    #[test]
    fn restore_normalizes_live_snapshot_to_session_ended() {
        let snapshot = TaskSnapshot {
            task_id: "monitor_old".into(),
            kind: TaskKind::MonitorCommand,
            owner_thread_id: "thread-old".into(),
            description: "old task".into(),
            status: TaskStatus::Running,
            created_at_ms: 1,
            ended_at_ms: None,
            event_count: 0,
            total_bytes: 0,
            exit_code: None,
            failure_summary: None,
            anchor_message_id: None,
            output_tail: String::new(),
        }
        .normalize_after_restore();
        assert_eq!(snapshot.status, TaskStatus::SessionEnded);
        assert!(snapshot.ended_at_ms.is_some());
        assert!(snapshot.failure_summary.is_some());
    }

    #[test]
    fn register_with_id_is_idempotent() {
        let id = TaskId("custom_42".into());
        let a = register_with_id(
            id.clone(),
            TaskKind::MonitorCommand,
            "thread-id".into(),
            "custom".into(),
            CancellationToken::new(),
        );
        let b = register_with_id(
            id.clone(),
            TaskKind::MonitorCommand,
            "thread-id".into(),
            "custom".into(),
            CancellationToken::new(),
        );
        // The second registration returns the same live registry entry.
        assert!(Arc::ptr_eq(&a, &b));
        let from_registry = get_by_str(&id.0).expect("id present in registry");
        assert!(Arc::ptr_eq(&a, &from_registry));
        remove(&id);
    }

    #[tokio::test]
    async fn stop_invokes_on_stop_hook() {
        let id = TaskId("hook_task".into());
        let proxy = register_with_id(
            id.clone(),
            TaskKind::MonitorCommand,
            "thread-hook".into(),
            "hook".into(),
            CancellationToken::new(),
        );
        let called: Arc<std::sync::Mutex<Option<String>>> = Arc::new(std::sync::Mutex::new(None));
        let called2 = Arc::clone(&called);
        proxy.set_on_stop(Arc::new(move |id, _| {
            *called2.lock().unwrap() = Some(id.to_string());
        }));
        stop(&id.0).await.expect("stop should succeed");
        assert_eq!(called.lock().unwrap().as_deref(), Some(id.0.as_str()));
        assert!(proxy.status().is_terminal());
        remove(&id);
    }

    /// The 8KB tail cut must land on a UTF-8 char boundary: CJK output past
    /// the cap used to panic split_off's is_char_boundary assertion.
    #[test]
    fn snapshot_tail_truncates_multibyte_output_on_char_boundary() {
        let id = TaskId("utf8_tail".into());
        let proxy = register_with_id(
            id.clone(),
            TaskKind::MonitorCommand,
            "thread-utf8".into(),
            "utf8".into(),
            CancellationToken::new(),
        );
        // Two 15KB CJK lines: the raw 8KB cut lands inside a 3-byte char.
        let line = "中".repeat(5000);
        proxy.push_event(&id, line.clone());
        proxy.push_event(&id, line);
        let tail = proxy.snapshot(&id).output_tail;
        assert!(tail.len() <= SNAPSHOT_TAIL_BYTES);
        assert!(tail.ends_with('中'));
        remove(&id);
    }

    /// Thread-lifetime cleanup releases the registry entries — the
    /// process-global map must not accumulate entries for disposed threads.
    #[test]
    fn cleanup_thread_removes_tasks() {
        let id = TaskId("cleanup_42".into());
        register_with_id(
            id.clone(),
            TaskKind::MonitorCommand,
            "t-cleanup".into(),
            "cleanup watcher".into(),
            CancellationToken::new(),
        );
        assert!(get_by_str(&id.0).is_some(), "task registered");

        cleanup_thread("t-cleanup");

        assert!(
            get_by_str(&id.0).is_none(),
            "task removed from the registry"
        );
    }

    /// `cancel_all_for_thread` is the cancel half of thread teardown —
    /// `cleanup_thread` only drops entries. A task owned by the deleting
    /// thread must have its token cancelled + status driven to SessionEnded,
    /// while another thread's task is untouched.
    #[tokio::test]
    async fn cancel_all_for_thread_cancels_owned_tasks() {
        use tokio_util::sync::CancellationToken;
        let cancel = CancellationToken::new();
        let (_id, task) = register("t-x1".into(), "test".into(), cancel.clone());
        assert!(!cancel.is_cancelled(), "pre: token not yet cancelled");
        cancel_all_for_thread("t-x1").await;
        assert!(cancel.is_cancelled(), "owned task's token was cancelled");
        assert_eq!(task.status(), TaskStatus::SessionEnded);

        // A different thread's task is not affected.
        let other = CancellationToken::new();
        let (_id2, _task2) = register("t-other".into(), "other".into(), other.clone());
        cancel_all_for_thread("t-x1").await; // already terminal — no-op
        assert!(!other.is_cancelled(), "other thread's task untouched");
    }

    /// `stop_all_for_thread` is the explicit user-cancel fan-out: owned
    /// tasks settle as `Stopped` (TaskStop semantics) with their token
    /// cancelled, while another thread's tasks stay untouched.
    #[tokio::test]
    async fn stop_all_for_thread_stops_owned_tasks() {
        use tokio_util::sync::CancellationToken;
        let thread_id = "t-stop-1";
        let cancel = CancellationToken::new();
        let (id, task) = register(thread_id.into(), "test".into(), cancel.clone());
        assert!(!cancel.is_cancelled(), "pre: token not yet cancelled");
        stop_all_for_thread(thread_id).await;
        assert!(cancel.is_cancelled(), "owned task's token was cancelled");
        assert_eq!(task.status(), TaskStatus::Stopped);
        let snap = task.snapshot(&id);
        assert_eq!(snap.status, TaskStatus::Stopped);

        // A different thread's task is not affected.
        let other = CancellationToken::new();
        let (_id2, task2) = register("t-stop-other".into(), "other".into(), other.clone());
        stop_all_for_thread(thread_id).await; // already terminal — no-op
        assert!(!other.is_cancelled(), "other thread's task untouched");
        assert_eq!(task2.status(), TaskStatus::Running);

        remove(&id);
        cleanup_thread("t-stop-other");
    }
}
