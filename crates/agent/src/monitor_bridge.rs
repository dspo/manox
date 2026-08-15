//! Bridge pi-path background tasks (Monitor command/WebSocket + background
//! Bash) into the legacy `background_task` registry and the facade's
//! `BackgroundTaskUpdated` event stream.
//!
//! One bridge per session orchestrator pair, spawned next to
//! `attach_orchestrators` in `pi_engine`. It subscribes to the monitor's
//! lifecycle + raw-output broadcasts and the background manager's lifecycle
//! broadcast, mirrors each pi task into the legacy registry (so `stop`,
//! `snapshots_for_thread`, and the gpui host's cards see one id space), and
//! re-emits every state change as `ThreadEvent::BackgroundTaskUpdated` via the
//! facade's `BackendNotice` channel. Exits when the orchestrator senders drop
//! (session torn down or replaced by a `NewSession`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_extensions::bash::orchestration::{BackgroundEvent, BackgroundManager};
use pi_extensions::monitor::{
    MonitorEvent, MonitorKind, MonitorManager, MonitorOutput, MonitorStatus,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::background_task::{self, BackgroundTask, TaskId, TaskKind, TaskStatus};
use crate::thread::ThreadEvent;
use crate::thread_engine::BackendNotice;

/// How often the background-Bash poller diffs output.
const BACKGROUND_POLL_INTERVAL: Duration = Duration::from_millis(500);
/// Tail bytes the poller requests from the registry per poll.
const POLL_TAIL_BYTES: usize = 32 * 1024;
/// Output lines collected before a monitor task re-emits its snapshot.
const OUTPUT_EMIT_THRESHOLD: u32 = 5;

/// Shared bridge state: the legacy proxies keyed by pi task id, plus per-task
/// output counters for emit throttling.
struct BridgeState {
    proxies: HashMap<String, Arc<BackgroundTask>>,
    output_since_emit: HashMap<String, u32>,
}

impl BridgeState {
    fn new() -> Self {
        Self {
            proxies: HashMap::new(),
            output_since_emit: HashMap::new(),
        }
    }
}

/// Spawn the bridge for one session's orchestrator pair.
pub fn spawn(
    monitor: Arc<MonitorManager>,
    background: Arc<BackgroundManager>,
    notice_tx: mpsc::UnboundedSender<BackendNotice>,
    owner_thread_id: String,
) {
    crate::runtime::handle().spawn(run(monitor, background, notice_tx, owner_thread_id));
}

async fn run(
    monitor: Arc<MonitorManager>,
    background: Arc<BackgroundManager>,
    notice_tx: mpsc::UnboundedSender<BackendNotice>,
    owner_thread_id: String,
) {
    let mut lifecycle = monitor.subscribe();
    let mut output = monitor.subscribe_output();
    let mut bg = background.subscribe();
    let state: Arc<Mutex<BridgeState>> = Arc::new(Mutex::new(BridgeState::new()));

    loop {
        tokio::select! {
            ev = lifecycle.recv() => {
                let Ok(ev) = ev else { break };
                handle_monitor_event(ev, &monitor, &state, &notice_tx, &owner_thread_id);
            }
            out = output.recv() => {
                let Ok(out) = out else { break };
                handle_monitor_output(out, &state, &notice_tx);
            }
            ev = bg.recv() => {
                let Ok(ev) = ev else { break };
                handle_background_event(ev, &background, &state, &notice_tx, &owner_thread_id);
            }
        }
    }
}

fn map_kind(kind: MonitorKind) -> TaskKind {
    match kind {
        MonitorKind::Command => TaskKind::MonitorCommand,
        MonitorKind::WebSocket => TaskKind::MonitorWebSocket,
    }
}

fn map_terminal(status: MonitorStatus) -> TaskStatus {
    match status {
        MonitorStatus::Running => TaskStatus::Running,
        MonitorStatus::Completed => TaskStatus::Completed,
        MonitorStatus::TimedOut => TaskStatus::TimedOut,
        MonitorStatus::Stopped => TaskStatus::Stopped,
        MonitorStatus::Failed => TaskStatus::Failed,
    }
}

/// Emit a `BackgroundTaskUpdated` notice for a proxy task.
fn emit_snapshot(
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
    proxy: &Arc<BackgroundTask>,
    id: &str,
) {
    let snapshot = proxy.snapshot(&TaskId(id.to_string()));
    let _ = notice_tx.send(BackendNotice::Event(Box::new(
        ThreadEvent::BackgroundTaskUpdated { snapshot },
    )));
}

fn handle_monitor_event(
    ev: MonitorEvent,
    monitor: &Arc<MonitorManager>,
    state: &Arc<Mutex<BridgeState>>,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
    owner_thread_id: &str,
) {
    match ev {
        MonitorEvent::Spawned {
            id,
            description,
            kind,
        } => {
            let monitor = Arc::clone(monitor);
            let on_stop: Arc<dyn Fn(&str) + Send + Sync> = Arc::new(move |id| monitor.stop(id));
            let proxy = register_proxy(
                &id,
                map_kind(kind),
                owner_thread_id,
                description,
                on_stop,
                state,
            );
            emit_snapshot(notice_tx, &proxy, &id);
        }
        MonitorEvent::Completed { id, exit_code } => {
            finish_monitor(
                &id,
                MonitorStatus::Completed,
                exit_code,
                None,
                state,
                notice_tx,
            );
        }
        MonitorEvent::TimedOut { id } => {
            finish_monitor(&id, MonitorStatus::TimedOut, None, None, state, notice_tx);
        }
        MonitorEvent::Stopped { id } => {
            finish_monitor(&id, MonitorStatus::Stopped, None, None, state, notice_tx);
        }
        MonitorEvent::Failed { id, reason } => {
            finish_monitor(
                &id,
                MonitorStatus::Failed,
                None,
                Some(reason),
                state,
                notice_tx,
            );
        }
        MonitorEvent::Killed { id } => {
            finish_monitor(&id, MonitorStatus::Stopped, None, None, state, notice_tx);
        }
    }
}

/// Register a proxy for a task and arm its stop hook.
fn register_proxy(
    id: &str,
    kind: TaskKind,
    owner_thread_id: &str,
    description: String,
    on_stop: Arc<dyn Fn(&str) + Send + Sync>,
    state: &Arc<Mutex<BridgeState>>,
) -> Arc<BackgroundTask> {
    let proxy = background_task::register_with_id(
        TaskId(id.to_string()),
        kind,
        owner_thread_id.to_string(),
        description,
        CancellationToken::new(),
    );
    proxy.set_on_stop(on_stop);
    state
        .lock()
        .expect("bridge state poisoned")
        .proxies
        .insert(id.to_string(), Arc::clone(&proxy));
    proxy
}

/// Drive a monitor task to its terminal state and emit the final snapshot.
fn finish_monitor(
    id: &str,
    status: MonitorStatus,
    exit_code: Option<i32>,
    failure_summary: Option<String>,
    state: &Arc<Mutex<BridgeState>>,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
) {
    let proxy = state
        .lock()
        .expect("bridge state poisoned")
        .proxies
        .remove(id);
    let Some(proxy) = proxy else { return };
    if let Some(code) = exit_code {
        proxy.set_exit_code(Some(code));
    }
    if let Some(reason) = failure_summary {
        proxy.set_failure_summary(reason);
    }
    proxy.push_terminal(&TaskId(id.to_string()), map_terminal(status));
    emit_snapshot(notice_tx, &proxy, id);
}

fn handle_monitor_output(
    out: MonitorOutput,
    state: &Arc<Mutex<BridgeState>>,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
) {
    let proxy = state
        .lock()
        .expect("bridge state poisoned")
        .proxies
        .get(&out.id)
        .cloned();
    let Some(proxy) = proxy else { return };
    proxy.push_event(&TaskId(out.id.clone()), out.line);
    let emit = {
        let mut st = state.lock().expect("bridge state poisoned");
        let count = st.output_since_emit.entry(out.id.clone()).or_insert(0);
        *count += 1;
        if *count >= OUTPUT_EMIT_THRESHOLD {
            *count = 0;
            true
        } else {
            false
        }
    };
    if emit {
        emit_snapshot(notice_tx, &proxy, &out.id);
    }
}

fn handle_background_event(
    ev: BackgroundEvent,
    background: &Arc<BackgroundManager>,
    state: &Arc<Mutex<BridgeState>>,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
    owner_thread_id: &str,
) {
    match ev {
        BackgroundEvent::Spawned { id, command } => {
            let tid = id.0.clone();
            let bg_for_stop = Arc::clone(background);
            let on_stop: Arc<dyn Fn(&str) + Send + Sync> =
                Arc::new(move |id| bg_for_stop.kill(&pi::TaskId(id.to_string())));
            let proxy = register_proxy(
                &tid,
                TaskKind::BackgroundBash,
                owner_thread_id,
                format!("background bash: {command}"),
                on_stop,
                state,
            );
            proxy.set_command(command.clone());
            emit_snapshot(notice_tx, &proxy, &tid);
            // Poll output in a dedicated task; lifecycle terminals arrive via
            // the broadcast below.
            spawn_background_poller(
                Arc::clone(background),
                proxy,
                tid,
                Arc::clone(state),
                notice_tx.clone(),
            );
        }
        BackgroundEvent::Completed { id, exit_code } => {
            let tid = id.0.clone();
            finish_background(
                &tid,
                TaskStatus::Completed,
                exit_code,
                None,
                state,
                notice_tx,
            );
        }
        BackgroundEvent::Killed { id } => {
            let tid = id.0.clone();
            finish_background(&tid, TaskStatus::Stopped, None, None, state, notice_tx);
        }
        BackgroundEvent::Failed { id, reason } => {
            let tid = id.0.clone();
            finish_background(
                &tid,
                TaskStatus::Failed,
                None,
                Some(reason),
                state,
                notice_tx,
            );
        }
    }
}

fn finish_background(
    id: &str,
    status: TaskStatus,
    exit_code: Option<i32>,
    failure_summary: Option<String>,
    state: &Arc<Mutex<BridgeState>>,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
) {
    let proxy = state
        .lock()
        .expect("bridge state poisoned")
        .proxies
        .remove(id);
    let Some(proxy) = proxy else { return };
    if let Some(code) = exit_code {
        proxy.set_exit_code(Some(code));
    }
    if let Some(reason) = failure_summary {
        proxy.set_failure_summary(reason);
    }
    proxy.push_terminal(&TaskId(id.to_string()), status);
    emit_snapshot(notice_tx, &proxy, id);
}

/// Poll one background task's output on an interval and re-emit snapshots as
/// new output appears; exits once the task reaches a terminal state (the
/// terminal event is emitted by the lifecycle broadcast).
fn spawn_background_poller(
    background: Arc<BackgroundManager>,
    proxy: Arc<BackgroundTask>,
    id: String,
    state: Arc<Mutex<BridgeState>>,
    notice_tx: mpsc::UnboundedSender<BackendNotice>,
) {
    crate::runtime::handle().spawn(async move {
        let mut seen = 0usize;
        loop {
            if proxy.status().is_terminal() {
                break;
            }
            tokio::time::sleep(BACKGROUND_POLL_INTERVAL).await;
            let Ok(info) = background.status(&pi::TaskId(id.clone()), POLL_TAIL_BYTES) else {
                continue;
            };
            let tail = info.output_tail;
            if tail.len() < seen {
                // The registry tail rotated past the cursor; re-sync from the
                // current tail rather than stalling output forever.
                if !tail.is_empty() {
                    proxy.push_event(&TaskId(id.clone()), tail.clone());
                    emit_snapshot(&notice_tx, &proxy, &id);
                }
                seen = tail.len();
            } else if tail.len() > seen {
                let new = tail[seen..].to_string();
                proxy.push_event(&TaskId(id.clone()), new);
                seen = tail.len();
                emit_snapshot(&notice_tx, &proxy, &id);
            }
            if !info.is_running {
                // Terminal bookkeeping is owned by the lifecycle broadcast;
                // the poller just stops watching output.
                break;
            }
        }
        drop(state);
    });
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use pi_extensions::bash::background::BackgroundRegistry;
    use pi_extensions::monitor::MonitorManager;
    use tokio::sync::mpsc;

    use super::*;

    /// A real command monitor surfaces in the legacy registry with its pi task
    /// id, and the bridge emits running → completed snapshots carrying output.
    #[tokio::test]
    async fn monitor_spawn_bridges_snapshots() {
        let monitor = Arc::new(MonitorManager::new(Arc::new(BackgroundRegistry::new())));
        let background = Arc::new(BackgroundManager::new(Arc::new(BackgroundRegistry::new())));
        let (notice_tx, mut notice_rx) = mpsc::unbounded_channel::<BackendNotice>();
        tokio::spawn(run(
            Arc::clone(&monitor),
            Arc::clone(&background),
            notice_tx,
            "t1".into(),
        ));
        // Let the bridge subscribe to the broadcasts before any spawn.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let tid = monitor
            .spawn_command(
                "bridge watcher".into(),
                "echo bridge-line".into(),
                &PathBuf::from("/tmp"),
                std::time::Duration::from_secs(30),
                false,
            )
            .unwrap();

        let mut saw_running = false;
        let mut saw_completed = false;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        while tokio::time::Instant::now() < deadline {
            let Ok(Some(notice)) =
                tokio::time::timeout(std::time::Duration::from_millis(500), notice_rx.recv()).await
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

        // The legacy registry sees the same id; a user stop routes through the
        // proxy's on_stop hook into the monitor manager.
        let proxy = background_task::get_by_str(&tid).expect("proxy registered");
        assert_eq!(proxy.owner_thread_id(), "t1");
        background_task::stop(&tid).await.ok();
        background_task::remove(&TaskId(tid.clone()));
    }
}
