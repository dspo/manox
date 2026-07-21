//! Session-scoped registry of background tasks — Bash (`run_in_background`),
//! command Monitor, and WebSocket Monitor — each with an owner thread, status,
//! cancel token, and bounded output.
//!
//! The registry is process-global (session-scoped, since the session == the
//! manox process). Each task is keyed by a unique id. Tasks persist after exit
//! so a final poll or status card can observe the terminal state; a periodic GC
//! sweep removes long-dead entries.
//!
//! Event injection: all tasks owned by a thread share one event channel.
//! When a task produces an external event (stdout line, WS text frame), it
//! pushes the event into that channel. The owning Thread drains the channel
//! at safe join points (idle → auto-wakeup, or running → steer queue) and
//! writes the event into the model's message history as an
//! untrusted-external-data notice.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

/// How long a completed task stays in the registry before GC sweeps it.
const GC_AFTER_EXIT: Duration = Duration::from_secs(300);

/// Hard cap on the accumulated event buffer per task.
const MAX_BUFFER_BYTES: usize = 256 * 1024;

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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskKind {
    MonitorCommand,
    MonitorWebSocket,
    BackgroundBash,
}

/// The terminal status of a background task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Running,
    Completed,
    Failed,
    TimedOut,
    Stopped,
    SessionEnded,
}

/// An external event produced by a background task.
#[derive(Debug, Clone)]
pub struct TaskEvent {
    pub task_id: TaskId,
    pub text: String,
    pub seq: u64,
    pub at: Instant,
}

/// Serialize an event for injection into the model's message history as
/// untrusted external data.
pub fn format_event_for_model(event: &TaskEvent, task_kind: &TaskKind) -> String {
    let source = match task_kind {
        TaskKind::MonitorCommand => "Monitor (command)",
        TaskKind::MonitorWebSocket => "Monitor (WebSocket)",
        TaskKind::BackgroundBash => "Background Bash",
    };
    format!(
        "⚠ External event from {source} `{task_id}` (seq {seq}): {text}",
        task_id = event.task_id.0,
        seq = event.seq,
        text = event.text
    )
}

/// Shared state for one background task.
struct TaskState {
    kind: TaskKind,
    owner_thread_id: String,
    status: TaskStatus,
    cancel: CancellationToken,
    event_count: u64,
    events: Vec<TaskEvent>,
    events_byte_count: usize,
    total_bytes: u64,
    created_at: Instant,
    exited_at: Option<Instant>,
    description: String,
    command: Option<String>,
    ws_url: Option<String>,
    driver_abort: Option<tokio::task::AbortHandle>,
    /// The shared event bus sender for the owning thread. All tasks owned by
    /// the same thread share this sender; the receiver lives in the thread's
    /// event-injection map.
    event_tx: Option<async_channel::Sender<TaskEvent>>,
}

/// A registered background task.
pub struct BackgroundTask {
    state: Arc<std::sync::Mutex<TaskState>>,
}

impl BackgroundTask {
    fn new(
        kind: TaskKind,
        owner_thread_id: String,
        description: String,
        cancel: CancellationToken,
        event_tx: async_channel::Sender<TaskEvent>,
    ) -> Self {
        Self {
            state: Arc::new(std::sync::Mutex::new(TaskState {
                kind,
                owner_thread_id,
                status: TaskStatus::Running,
                cancel,
                event_count: 0,
                events: Vec::new(),
                events_byte_count: 0,
                total_bytes: 0,
                created_at: Instant::now(),
                exited_at: None,
                description,
                command: None,
                ws_url: None,
                driver_abort: None,
                event_tx: Some(event_tx),
            })),
        }
    }

    pub fn status(&self) -> TaskStatus {
        self.state.lock().expect("task state poisoned").status
    }

    pub fn is_running(&self) -> bool {
        self.state.lock().expect("task state poisoned").status == TaskStatus::Running
    }

    pub fn cancel(&self) {
        self.state
            .lock()
            .expect("task state poisoned")
            .cancel
            .cancel();
    }

    pub fn event_count(&self) -> u64 {
        self.state.lock().expect("task state poisoned").event_count
    }

    pub fn created_at(&self) -> Instant {
        self.state.lock().expect("task state poisoned").created_at
    }

    pub fn description(&self) -> String {
        self.state
            .lock()
            .expect("task state poisoned")
            .description
            .clone()
    }

    pub fn kind(&self) -> TaskKind {
        self.state.lock().expect("task state poisoned").kind.clone()
    }

    pub fn command(&self) -> Option<String> {
        self.state
            .lock()
            .expect("task state poisoned")
            .command
            .clone()
    }

    pub fn ws_url(&self) -> Option<String> {
        self.state
            .lock()
            .expect("task state poisoned")
            .ws_url
            .clone()
    }

    pub fn owner_thread_id(&self) -> String {
        self.state
            .lock()
            .expect("task state poisoned")
            .owner_thread_id
            .clone()
    }

    /// Push an event into the task's buffer and forward it to the owning thread.
    /// The ring buffer evicts the oldest events when the byte cap is exceeded,
    /// tracking the byte count correctly so it doesn't clear everything.
    /// When the shared event channel is full, the event is still recorded in
    /// the task's own ring buffer so the thread can replay on drain.
    pub fn push_event(&self, event: TaskEvent) {
        let mut s = self.state.lock().expect("task state poisoned");
        let evt_len = event.text.len();
        s.events.push(event.clone());
        s.events_byte_count += evt_len;
        while s.events_byte_count > MAX_BUFFER_BYTES && !s.events.is_empty() {
            let removed = s.events.remove(0);
            s.events_byte_count = s.events_byte_count.saturating_sub(removed.text.len());
        }
        s.event_count += 1;
        s.total_bytes = s.total_bytes.saturating_add(evt_len as u64);
        if let Some(tx) = &s.event_tx {
            let _ = tx.try_send(event);
        }
    }

    pub fn recent_events(&self) -> Vec<TaskEvent> {
        self.state
            .lock()
            .expect("task state poisoned")
            .events
            .clone()
    }

    pub fn total_bytes(&self) -> u64 {
        self.state.lock().expect("task state poisoned").total_bytes
    }

    /// Set the task to a terminal status. Idempotent: only the first call
    /// takes effect.
    pub fn set_terminal(&self, status: TaskStatus) {
        let mut s = self.state.lock().expect("task state poisoned");
        if s.status != TaskStatus::Running {
            return;
        }
        s.status = status;
        s.exited_at = Some(Instant::now());
        s.event_tx = None;
    }

    pub fn set_command(&self, cmd: String) {
        self.state.lock().expect("task state poisoned").command = Some(cmd);
    }

    pub fn set_ws_url(&self, url: String) {
        self.state.lock().expect("task state poisoned").ws_url = Some(url);
    }

    pub fn set_driver_abort(&self, handle: tokio::task::AbortHandle) {
        self.state.lock().expect("task state poisoned").driver_abort = Some(handle);
    }
}

/// The process-global background task registry.
struct Registry {
    tasks: HashMap<String, Arc<BackgroundTask>>,
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

/// Allocate a unique id for the given kind.
fn next_id(kind: &TaskKind) -> TaskId {
    let mut reg = registry().lock().expect("registry poisoned");
    let n = reg.next_id;
    reg.next_id += 1;
    let prefix = match kind {
        TaskKind::MonitorCommand => "monitor",
        TaskKind::MonitorWebSocket => "ws",
        TaskKind::BackgroundBash => "bash",
    };
    TaskId::new(prefix, n)
}

/// Per-thread event bus: one shared channel per thread, created lazily.
/// All tasks owned by the same thread send into this channel; the Thread
/// drains it at safe join points.
/// The shared event channel for a thread (sender, receiver).
type ThreadEventChannel = (
    async_channel::Sender<TaskEvent>,
    async_channel::Receiver<TaskEvent>,
);

/// Per-thread event bus: one shared channel per thread, created lazily.
static THREAD_EVENT_BUS: OnceLock<std::sync::Mutex<HashMap<String, ThreadEventChannel>>> =
    OnceLock::new();

fn thread_event_bus() -> &'static std::sync::Mutex<HashMap<String, ThreadEventChannel>> {
    THREAD_EVENT_BUS.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// Get or create the shared event channel for a thread. All tasks owned by
/// the same thread share this channel.
pub fn ensure_thread_event_bus(
    thread_id: &str,
) -> (
    async_channel::Sender<TaskEvent>,
    async_channel::Receiver<TaskEvent>,
) {
    let mut map = thread_event_bus().lock().expect("event bus poisoned");
    map.entry(thread_id.to_string())
        .or_insert_with(|| async_channel::bounded(1024))
        .clone()
}

/// Register a task with an externally-assigned id (e.g. background Bash's
/// `bash_N` from the old registry). The task is registered under the given
/// id so TaskStop can find it.
pub fn register_with_id(
    id: String,
    kind: TaskKind,
    owner_thread_id: String,
    description: String,
    cancel: CancellationToken,
) -> Arc<BackgroundTask> {
    let (event_tx, _) = ensure_thread_event_bus(&owner_thread_id);
    let task = Arc::new(BackgroundTask::new(
        kind,
        owner_thread_id,
        description,
        cancel,
        event_tx,
    ));
    let mut reg = registry().lock().expect("registry poisoned");
    reg.tasks.insert(id, task.clone());
    task
}

/// Take the receiver for a thread's event bus. Returns None if the bus was
/// never created.
pub fn take_thread_event_rx(thread_id: &str) -> Option<async_channel::Receiver<TaskEvent>> {
    thread_event_bus()
        .lock()
        .expect("event bus poisoned")
        .remove(thread_id)
        .map(|(_, rx)| rx)
}

/// Drop the event bus for a thread (called when the thread is dropped).
pub fn remove_thread_event_bus(thread_id: &str) {
    thread_event_bus()
        .lock()
        .expect("event bus poisoned")
        .remove(thread_id);
}

/// Register a new background task and return its id and handle.
pub fn register(
    kind: TaskKind,
    owner_thread_id: String,
    description: String,
    cancel: CancellationToken,
) -> (TaskId, Arc<BackgroundTask>) {
    let (event_tx, _) = ensure_thread_event_bus(&owner_thread_id);
    let id = next_id(&kind);
    let task = Arc::new(BackgroundTask::new(
        kind,
        owner_thread_id,
        description,
        cancel,
        event_tx,
    ));
    let mut reg = registry().lock().expect("registry poisoned");
    reg.tasks.insert(id.0.clone(), task.clone());
    (id, task)
}

/// Look up a task by id.
pub fn get(id: &TaskId) -> Option<Arc<BackgroundTask>> {
    registry()
        .lock()
        .expect("registry poisoned")
        .tasks
        .get(&id.0)
        .cloned()
}

/// Remove a task from the registry. Used when setup fails before the driver
/// starts.
pub fn remove(id: &TaskId) {
    registry()
        .lock()
        .expect("registry poisoned")
        .tasks
        .remove(&id.0);
}

/// Build a summary of currently running tasks from within a held registry lock.
fn list_stoppable_under_lock(reg: &Registry) -> String {
    if reg.tasks.is_empty() {
        return "No background tasks are currently running.".into();
    }
    let mut lines: Vec<String> = Vec::new();
    for (id, task) in &reg.tasks {
        let s = task.state.lock().expect("task state poisoned");
        if s.status == TaskStatus::Running {
            let kind_str = match s.kind {
                TaskKind::MonitorCommand => "monitor (command)",
                TaskKind::MonitorWebSocket => "monitor (WebSocket)",
                TaskKind::BackgroundBash => "background bash",
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

/// Stop a task by id. Returns Ok(()) if the task was found and stopped (or
/// was already terminal). Returns Err with a summary of available tasks if
/// the id is unknown.
pub fn stop(id: &str) -> Result<(), String> {
    let reg = registry().lock().expect("registry poisoned");
    let Some(task) = reg.tasks.get(id) else {
        let summary = list_stoppable_under_lock(&reg);
        return Err(format!("Unknown task id: {id}. {summary}"));
    };
    let task = task.clone();
    drop(reg);

    let s = task.state.lock().expect("task state poisoned");
    if s.status != TaskStatus::Running {
        return Ok(());
    }
    let driver_abort = s.driver_abort.clone();
    drop(s);

    task.cancel();
    task.set_terminal(TaskStatus::Stopped);
    if let Some(handle) = driver_abort {
        handle.abort();
    }
    Ok(())
}

/// Run a garbage-collection pass: remove tasks whose process exited more than
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

/// Cancel all tasks owned by a thread. Called when the thread is archived or
/// dropped.
pub fn cancel_all_for_thread(thread_id: &str) {
    let reg = registry().lock().expect("registry poisoned");
    for task in reg.tasks.values() {
        let s = task.state.lock().expect("task state poisoned");
        if s.owner_thread_id == thread_id && s.status == TaskStatus::Running {
            drop(s);
            task.cancel();
            task.set_terminal(TaskStatus::SessionEnded);
        }
    }
}

/// Whether a thread has any running tasks. The UI uses this to keep the
/// thread alive during task switches.
pub fn thread_has_running_tasks(thread_id: &str) -> bool {
    let reg = registry().lock().expect("registry poisoned");
    reg.tasks.values().any(|task| {
        let s = task.state.lock().expect("task state poisoned");
        s.owner_thread_id == thread_id && s.status == TaskStatus::Running
    })
}

/// Cancel all running tasks across all threads. Called at application exit.
pub fn shutdown_all() {
    let reg = registry().lock().expect("registry poisoned");
    for task in reg.tasks.values() {
        let s = task.state.lock().expect("task state poisoned");
        if s.status == TaskStatus::Running {
            drop(s);
            task.cancel();
            task.set_terminal(TaskStatus::SessionEnded);
        }
    }
}

/// Remove all tasks owned by a thread. Called when the thread is archived.
pub fn remove_all_for_thread(thread_id: &str) {
    let mut reg = registry().lock().expect("registry poisoned");
    reg.tasks
        .retain(|_, task| task.owner_thread_id() != thread_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_and_get() {
        let cancel = CancellationToken::new();
        let (id, task) = register(
            TaskKind::MonitorCommand,
            "thread-1".into(),
            "watch build".into(),
            cancel,
        );
        assert!(id.0.starts_with("monitor_"));
        assert_eq!(task.status(), TaskStatus::Running);
        assert_eq!(task.event_count(), 0);

        let found = get(&id).expect("should find task");
        assert_eq!(found.status(), TaskStatus::Running);
    }

    #[test]
    fn stop_and_terminal() {
        let cancel = CancellationToken::new();
        let (id, task) = register(
            TaskKind::MonitorCommand,
            "thread-1".into(),
            "test".into(),
            cancel,
        );
        assert_eq!(task.status(), TaskStatus::Running);

        stop(&id.0).expect("stop should succeed");
        assert_eq!(task.status(), TaskStatus::Stopped);

        stop(&id.0).expect("stop should succeed");
        assert_eq!(task.status(), TaskStatus::Stopped);
    }

    #[test]
    fn stop_unknown_task_returns_error_with_list() {
        let result = stop("nonexistent_id");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("Unknown task id"));
        assert!(err.contains("background tasks"));
    }

    #[test]
    fn push_event_updates_count() {
        let cancel = CancellationToken::new();
        let (_id, task) = register(
            TaskKind::MonitorCommand,
            "thread-1".into(),
            "test".into(),
            cancel,
        );
        task.push_event(TaskEvent {
            task_id: TaskId::new("monitor", 1),
            text: "hello".into(),
            seq: 1,
            at: Instant::now(),
        });
        assert_eq!(task.event_count(), 1);
        assert_eq!(task.total_bytes(), 5);
    }

    #[test]
    fn set_terminal_is_idempotent() {
        let cancel = CancellationToken::new();
        let (_id, task) = register(
            TaskKind::MonitorCommand,
            "thread-1".into(),
            "test".into(),
            cancel,
        );
        task.set_terminal(TaskStatus::Completed);
        assert_eq!(task.status(), TaskStatus::Completed);
        task.set_terminal(TaskStatus::Failed);
        assert_eq!(task.status(), TaskStatus::Completed);
    }

    #[test]
    fn ring_buffer_evicts_oldest_not_all() {
        let cancel = CancellationToken::new();
        let (_id, task) = register(
            TaskKind::MonitorCommand,
            "thread-1".into(),
            "test".into(),
            cancel,
        );

        let big = "X".repeat(200 * 1024);
        task.push_event(TaskEvent {
            task_id: TaskId::new("monitor", 1),
            text: big.clone(),
            seq: 1,
            at: Instant::now(),
        });
        task.push_event(TaskEvent {
            task_id: TaskId::new("monitor", 1),
            text: big.clone(),
            seq: 2,
            at: Instant::now(),
        });
        task.push_event(TaskEvent {
            task_id: TaskId::new("monitor", 1),
            text: "last".into(),
            seq: 3,
            at: Instant::now(),
        });

        let events = task.recent_events();
        let texts: Vec<&str> = events.iter().map(|e| e.text.as_str()).collect();
        assert!(
            texts.contains(&"last"),
            "last event should survive, got: {:?}",
            texts
        );
    }

    #[test]
    fn shared_event_bus_per_thread() {
        // Two tasks on the same thread share the same event bus.
        let (tx1, _rx1) = ensure_thread_event_bus("thread-a");
        let (tx2, _rx2) = ensure_thread_event_bus("thread-a");
        tx1.try_send(TaskEvent {
            task_id: TaskId::new("monitor", 1),
            text: "from tx1".into(),
            seq: 1,
            at: Instant::now(),
        })
        .expect("tx1 send");
        tx2.try_send(TaskEvent {
            task_id: TaskId::new("monitor", 2),
            text: "from tx2".into(),
            seq: 1,
            at: Instant::now(),
        })
        .expect("tx2 send");
        let (_, rx) = ensure_thread_event_bus("thread-a");
        let e1 = rx.try_recv().expect("first event");
        let e2 = rx.try_recv().expect("second event");
        assert_eq!(e1.text, "from tx1");
        assert_eq!(e2.text, "from tx2");
        remove_thread_event_bus("thread-a");
    }
}
