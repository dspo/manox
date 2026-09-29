//! Bridge pi-path background tasks (Monitor command/WebSocket + background
//! Bash) into the host's `background_task` registry and the facade's
//! `BackgroundTaskUpdated` event stream.
//!
//! The bridge is the session producers' `TaskObserver`: each `Spawned`
//! registers a proxy task under the pi task id (so `stop` and the host
//! cards see one id space, with an on_stop hook back into the producer),
//! each `Output` lands in the proxy's bounded ring (snapshots throttled),
//! and each `Settled` maps onto the wire-stable terminal status (cause
//! `Teardown` becomes `SessionEnded`). Output flows push-only — there is
//! no polling and no lagged broadcast to survive.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use manox_harness::bash::orchestration::BackgroundManager;
use manox_harness::monitor::MonitorManager;
use manox_harness::tasks::{Settlement, SettlementCause, SettlementKind, TaskFamily, TaskObserver};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::background_task::{self, TaskId, TaskKind, TaskStatus};
use crate::thread::ThreadEvent;
use crate::thread_engine::BackendNotice;

/// Output lines collected before a monitor task re-emits its snapshot.
const OUTPUT_EMIT_THRESHOLD: u32 = 5;

/// Shared bridge state: per-task output counters for emit throttling.
#[derive(Default)]
struct BridgeState {
    output_since_emit: Mutex<HashMap<String, u32>>,
}

/// The host's lifecycle observer for one session's pi-path producers.
struct BridgeObserver {
    state: Arc<BridgeState>,
    notice_tx: mpsc::UnboundedSender<BackendNotice>,
    owner_thread_id: String,
}

impl TaskObserver for BridgeObserver {
    fn on_spawned(
        &self,
        id: &str,
        family: TaskFamily,
        label: &str,
        stop: manox_harness::tasks::StopHandle,
    ) {
        let proxy = background_task::register_with_id(
            TaskId(id.to_string()),
            map_kind(family),
            self.owner_thread_id.clone(),
            label.to_string(),
            CancellationToken::new(),
        );
        proxy.set_on_stop(Arc::new(move |_| stop()));
        self.emit_snapshot(&proxy, id);
    }

    fn on_output(&self, id: &str, line: String) {
        let Some(proxy) = background_task::get_by_str(id) else {
            return;
        };
        proxy.push_event(&TaskId(id.to_string()), line);
        let emit = {
            let mut st = self.state.output_since_emit.lock().expect("state poisoned");
            let count = st.entry(id.to_string()).or_insert(0);
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

    fn on_settled(&self, id: &str, settlement: &Settlement) {
        let Some(proxy) = background_task::get_by_str(id) else {
            return;
        };
        if let Some(code) = settlement.exit_code {
            proxy.set_exit_code(Some(code));
        }
        if let Some(reason) = &settlement.failure_summary {
            proxy.set_failure_summary(reason.clone());
        }
        proxy.push_terminal(&TaskId(id.to_string()), map_status(settlement));
        self.emit_snapshot(&proxy, id);
    }
}

impl BridgeObserver {
    /// Emit a `BackgroundTaskUpdated` notice for a proxy task.
    fn emit_snapshot(&self, proxy: &background_task::BackgroundTask, id: &str) {
        let snapshot = proxy.snapshot(&TaskId(id.to_string()));
        let _ = self.notice_tx.send(BackendNotice::Event(Box::new(
            ThreadEvent::BackgroundTaskUpdated { snapshot },
        )));
    }
}

fn map_kind(family: TaskFamily) -> TaskKind {
    match family {
        TaskFamily::MonitorCommand => TaskKind::MonitorCommand,
        TaskFamily::MonitorWebSocket => TaskKind::MonitorWebSocket,
        TaskFamily::BackgroundBash => TaskKind::BackgroundBash,
        TaskFamily::Subagent => TaskKind::Subagent,
    }
}

/// The wire-stable status mapping. Cause `Teardown` settles as
/// `SessionEnded` — the session the card belonged to is going away.
fn map_status(settlement: &Settlement) -> TaskStatus {
    match settlement.kind {
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
    }
}

/// Bind the session's producers to a bridge observer. Called next to
/// `attach_orchestrators` at session build/restore; the observer lives as
/// long as the managers hold it.
pub fn spawn(
    monitor: Arc<MonitorManager>,
    background: Arc<BackgroundManager>,
    notice_tx: mpsc::UnboundedSender<BackendNotice>,
    owner_thread_id: String,
) {
    let observer = Arc::new(BridgeObserver {
        state: Arc::new(BridgeState::default()),
        notice_tx,
        owner_thread_id,
    });
    monitor.set_observer(Arc::clone(&observer) as Arc<dyn TaskObserver>);
    background.set_observer(observer);
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    use manox_harness::bash::background::BackgroundRegistry;
    use manox_harness::monitor::MonitorManager;
    use tokio::sync::mpsc;

    use super::*;

    /// A real command monitor surfaces in the host registry with its pi task
    /// id, and the bridge emits running → completed snapshots carrying
    /// output; a user stop routes through the proxy's on_stop hook into the
    /// monitor manager.
    #[tokio::test]
    async fn monitor_spawn_bridges_snapshots() {
        let monitor = MonitorManager::new(Arc::new(BackgroundRegistry::new()));
        let background = BackgroundManager::new(Arc::new(BackgroundRegistry::new()));
        let (notice_tx, mut notice_rx) = mpsc::unbounded_channel::<BackendNotice>();
        spawn(
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

        // The host registry sees the same id; a user stop routes through the
        // proxy's on_stop hook into the monitor manager.
        let proxy = background_task::get_by_str(&tid).expect("proxy registered");
        assert_eq!(proxy.owner_thread_id(), "t1");
        background_task::stop(&tid).await.ok();
        background_task::remove(&TaskId(tid.clone()));
    }

    /// A settled teardown causes maps to `SessionEnded` on the wire — the
    /// card belongs to a session that is going away.
    #[test]
    fn teardown_settlement_maps_to_session_ended() {
        let settlement = Settlement::new(SettlementKind::Stopped, SettlementCause::Teardown);
        assert_eq!(map_status(&settlement), TaskStatus::SessionEnded);
        let user_stop = Settlement::new(SettlementKind::Stopped, SettlementCause::UserStop);
        assert_eq!(map_status(&user_stop), TaskStatus::Stopped);
        let completed = Settlement::new(SettlementKind::Completed, SettlementCause::Natural);
        assert_eq!(map_status(&completed), TaskStatus::Completed);
    }
}
