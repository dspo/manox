// Background orchestration — the runtime half that binds background tasks
// to an agent session.
//
// The execution engine (`BackgroundRegistry`) spawns, polls, and kills; this
// module closes the loop with the agent runtime:
//
//   1. task → model: a completed task steers a summary into the agent's
//      context at the next tool-call boundary (the model can still fetch the
//      full output via `bash_output`, whose read cursor is untouched).
//   2. run → task: an aborted run kills its background tasks; a settled run
//      keeps them so a long task survives across turns. The caller kills
//      everything on session teardown via [`BackgroundManager::kill_all`].
//   3. task → events: a `BackgroundEvent` stream for UI / audit consumers.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use pi::BackgroundTaskRegistry;
use pi::coding_agent::AgentSession;
use pi::harness::{HarnessListener, HarnessSubscription};
use pi::types::AgentMessage;
use tokio::sync::broadcast;

use super::background::{BackgroundRegistry, TaskStatusInfo};

/// How a completion summary reaches the bound session (`HarnessHandle::steer`).
type Steerer = Arc<dyn Fn(AgentMessage) + Send + Sync>;

/// Tail size of a task's output included in the completion summary.
const SUMMARY_TAIL_BYTES: usize = 2 * 1024;

/// Lifecycle events of a background task, for UI / audit consumers.
#[derive(Debug, Clone)]
pub enum BackgroundEvent {
    Spawned {
        id: pi::TaskId,
        command: String,
    },
    Completed {
        id: pi::TaskId,
        exit_code: Option<i32>,
    },
    Killed {
        id: pi::TaskId,
    },
    Failed {
        id: pi::TaskId,
        reason: String,
    },
}

/// Orchestrates background tasks against one agent session.
///
/// Not `Clone`-cheap by design: one manager per session. `spawn` goes
/// through the registry but also registers the task with this run, watches
/// for completion, steers a summary into the bound session, and emits
/// events. Without a bound steerer the task still runs and emits events —
/// only the model injection is skipped.
pub struct BackgroundManager {
    pub(crate) registry: Arc<BackgroundRegistry>,
    /// How a completion summary reaches the model (`HarnessHandle::steer`).
    /// Guarded so a task spawned before `attach` can still steer once a
    /// session is bound.
    steerer: Arc<Mutex<Option<Steerer>>>,
    /// Tokio handle captured at construction, used to spawn the abort
    /// cleanup from the listener (which may run on any thread).
    runtime: Option<tokio::runtime::Handle>,
    event_tx: broadcast::Sender<BackgroundEvent>,
    /// Tasks owned by this manager, killed together on abort / teardown.
    tasks: Arc<Mutex<HashSet<pi::TaskId>>>,
    /// Lifecycle subscription; dropped with the manager. Guarded so the
    /// manager can be shared behind an `Arc` (a bash tool holds it) while
    /// still attaching to a session.
    _lifecycle: Arc<Mutex<Option<HarnessSubscription>>>,
}

impl BackgroundManager {
    pub fn new(registry: Arc<BackgroundRegistry>) -> Self {
        let (event_tx, _) = broadcast::channel(64);
        BackgroundManager {
            registry,
            steerer: Arc::new(Mutex::new(None)),
            runtime: tokio::runtime::Handle::try_current().ok(),
            event_tx,
            tasks: Arc::new(Mutex::new(HashSet::new())),
            _lifecycle: Arc::new(Mutex::new(None)),
        }
    }

    /// Bind an agent session: steer completions into it and cancel this
    /// manager's tasks when the run is aborted.
    ///
    /// Re-entrant: calling `attach` again replaces the previous steerer and
    /// lifecycle subscription (the old subscription drops and unsubscribes).
    pub fn attach(&self, session: &mut AgentSession) {
        let handle = session.handle();
        *self.steerer.lock().expect("steerer lock poisoned") = Some(Arc::new(move |message| {
            handle.steer(message);
        }));
        let registry = Arc::clone(&self.registry);
        let tasks = Arc::clone(&self.tasks);
        let event_tx = self.event_tx.clone();
        // Refresh the handle in case construction happened outside a runtime;
        // the listener itself may run on any thread.
        let runtime = self
            .runtime
            .clone()
            .or_else(|| tokio::runtime::Handle::try_current().ok());
        let listener: HarnessListener = Arc::new(move |event| {
            if matches!(event, pi::harness::HarnessEvent::Abort { .. }) {
                let registry = Arc::clone(&registry);
                let tasks = Arc::clone(&tasks);
                let event_tx = event_tx.clone();
                match &runtime {
                    Some(runtime) => {
                        runtime.spawn(async move {
                            kill_all_tasks(&registry, &tasks, &event_tx).await;
                        });
                    }
                    None => tracing::warn!(
                        "abort received outside a tokio runtime; background tasks not cancelled"
                    ),
                }
            }
        });
        *self._lifecycle.lock().expect("lifecycle lock poisoned") =
            Some(session.subscribe_harness(listener));
    }

    /// Start a background task under this manager and watch it to completion.
    pub fn spawn(&self, command: &str, cwd: &std::path::Path) -> Result<pi::TaskId, pi::TaskError> {
        let id = self.registry.spawn(command, cwd)?;
        self.tasks
            .lock()
            .expect("tasks lock poisoned")
            .insert(id.clone());
        let _ = self.event_tx.send(BackgroundEvent::Spawned {
            id: id.clone(),
            command: command.to_string(),
        });

        let registry = Arc::clone(&self.registry);
        let steerer = Arc::clone(&self.steerer);
        let event_tx = self.event_tx.clone();
        let tasks = Arc::clone(&self.tasks);
        let tid = id.clone();
        tokio::spawn(async move {
            // Event-driven: the drain task notifies the moment the exit is
            // recorded, so no polling interval delays the completion.
            if registry.wait_exit(&tid).await.is_err() {
                let _ = event_tx.send(BackgroundEvent::Failed {
                    id: tid.clone(),
                    reason: "task disappeared before exit".into(),
                });
                return;
            }
            let status = match registry.status(&tid, SUMMARY_TAIL_BYTES) {
                Ok(status) => status,
                Err(e) => {
                    let _ = event_tx.send(BackgroundEvent::Failed {
                        id: tid.clone(),
                        reason: e.to_string(),
                    });
                    return;
                }
            };
            // Exactly-once: `kill_all` (abort / teardown) drains the set
            // first, so a killed task must not steer a "completed" summary
            // into the session.
            if tasks.lock().expect("tasks lock poisoned").remove(&tid) {
                let steerer = steerer.lock().expect("steerer lock poisoned").clone();
                finish_task(&tid, &status, steerer.as_ref(), &event_tx);
            }
        });
        Ok(id)
    }

    /// Cancel every task this manager owns and emit `Killed` for each.
    pub async fn kill_all(&self) {
        kill_all_tasks(&self.registry, &self.tasks, &self.event_tx).await;
    }

    /// Subscribe to background events; dropping the receiver unsubscribes.
    pub fn subscribe(&self) -> broadcast::Receiver<BackgroundEvent> {
        self.event_tx.subscribe()
    }
}

/// Steer a completion summary into the bound session and emit the event.
fn finish_task(
    id: &pi::TaskId,
    status: &TaskStatusInfo,
    steerer: Option<&Steerer>,
    event_tx: &broadcast::Sender<BackgroundEvent>,
) {
    if let Some(steer) = steerer {
        steer(AgentMessage::user(format_summary(id, status)));
    }
    let _ = event_tx.send(BackgroundEvent::Completed {
        id: id.clone(),
        exit_code: status.exit_code.flatten(),
    });
}

/// Build the model-facing completion notice.
fn format_summary(id: &pi::TaskId, status: &TaskStatusInfo) -> String {
    let code = match status.exit_code {
        Some(Some(code)) => format!("exit code {code}"),
        Some(None) => "terminated by signal".to_string(),
        None => "finished".to_string(),
    };
    let tail = if status.output_tail.trim().is_empty() {
        String::new()
    } else {
        format!("\n\nRecent output:\n{}", status.output_tail.trim_end())
    };
    format!(
        "Background task `{id}` completed ({code}).{tail}\n\nUse `BashOutput` (shell_id: \"{id}\") for the full output."
    )
}

async fn kill_all_tasks(
    registry: &BackgroundRegistry,
    tasks: &Mutex<HashSet<pi::TaskId>>,
    event_tx: &broadcast::Sender<BackgroundEvent>,
) {
    let ids: Vec<pi::TaskId> = tasks.lock().expect("tasks lock poisoned").drain().collect();
    for id in ids {
        let _ = registry.kill(&id).await;
        let _ = event_tx.send(BackgroundEvent::Killed { id });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi::types::ContentBlock;
    use std::path::Path;
    use std::time::Duration;

    fn new_manager() -> BackgroundManager {
        BackgroundManager::new(Arc::new(BackgroundRegistry::new()))
    }

    #[test]
    fn summary_reports_id_and_exit_code() {
        let status = TaskStatusInfo {
            is_running: false,
            exit_code: Some(Some(3)),
            output_tail: "boom".into(),
        };
        let summary = format_summary(&pi::TaskId("bg_1".into()), &status);
        assert!(summary.contains("bg_1"));
        assert!(summary.contains("exit code 3"));
        assert!(summary.contains("Recent output:\nboom"));
        assert!(summary.contains("BashOutput"));
    }

    #[test]
    fn summary_omits_empty_tail() {
        let status = TaskStatusInfo {
            is_running: false,
            exit_code: Some(None),
            output_tail: String::new(),
        };
        let summary = format_summary(&pi::TaskId("bg_1".into()), &status);
        assert!(summary.contains("terminated by signal"));
        assert!(!summary.contains("Recent output"));
    }

    #[tokio::test]
    async fn spawn_emits_events_and_completion_steers() {
        let manager = new_manager();
        // Inject a recording steerer (tests run without an agent session).
        let seen: Arc<Mutex<Vec<AgentMessage>>> = Arc::new(Mutex::new(Vec::new()));
        let seen2 = Arc::clone(&seen);
        let steerer: Steerer = Arc::new(move |m| seen2.lock().unwrap().push(m));
        *manager.steerer.lock().unwrap() = Some(steerer);

        let mut rx = manager.subscribe();
        let id = manager
            .spawn("echo hello; sleep 0.1", Path::new("/tmp"))
            .unwrap();

        // Blocking receives let the async watcher run; bound each wait.
        let mut saw_spawned = false;
        let mut saw_completed = false;
        for _ in 0..10 {
            match tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
                Ok(Ok(BackgroundEvent::Spawned { .. })) => saw_spawned = true,
                Ok(Ok(BackgroundEvent::Completed { .. })) => {
                    saw_completed = true;
                    break;
                }
                Ok(Ok(_)) => {}
                _ => break,
            }
        }
        assert!(saw_spawned, "spawned event observed");
        assert!(saw_completed, "completed event observed");

        // The completion summary reached the steerer.
        let messages = seen.lock().unwrap();
        let summary = messages.iter().find_map(|m| match m {
            AgentMessage::User { content, .. } => content.iter().find_map(|b| match b {
                ContentBlock::Text { text, .. } => Some(text.clone()),
                _ => None,
            }),
            _ => None,
        });
        let summary = summary.expect("steered a completion message");
        assert!(summary.contains(&id.0), "summary names the task: {summary}");
        assert!(summary.contains("BashOutput"));
    }

    #[tokio::test]
    async fn kill_all_cancels_tasks_and_emits_killed() {
        let manager = new_manager();
        let mut rx = manager.subscribe();
        let id = manager.spawn("sleep 30", Path::new("/tmp")).unwrap();
        assert!(manager.registry.status(&id, 0).unwrap().is_running);

        manager.kill_all().await;

        let mut saw_killed = false;
        for _ in 0..10 {
            match tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
                Ok(Ok(BackgroundEvent::Killed { .. })) => {
                    saw_killed = true;
                    break;
                }
                Ok(Ok(_)) => {}
                _ => break,
            }
        }
        assert!(saw_killed, "killed event observed");
        // The registry entry lingers until GC; the process exit is recorded
        // asynchronously by the drain task, so wait for it.
        let mut cancelled = false;
        for _ in 0..50 {
            if !manager.registry.status(&id, 0).unwrap().is_running {
                cancelled = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(cancelled, "task cancelled");
    }

    /// Regression: a task killed via `kill_all` must neither steer a
    /// completion summary nor emit `Completed` — the exactly-once witness in
    /// the watcher drains the same set.
    #[tokio::test]
    async fn killed_task_does_not_steer_or_complete() {
        let manager = new_manager();
        let seen: Arc<Mutex<Vec<AgentMessage>>> = Arc::new(Mutex::new(Vec::new()));
        let seen2 = Arc::clone(&seen);
        let steerer: Steerer = Arc::new(move |m| seen2.lock().unwrap().push(m));
        *manager.steerer.lock().unwrap() = Some(steerer);

        let mut rx = manager.subscribe();
        let _id = manager.spawn("sleep 30", Path::new("/tmp")).unwrap();
        manager.kill_all().await;
        // Give the watcher a poll cycle to observe the exit.
        tokio::time::sleep(Duration::from_millis(300)).await;

        let mut saw_completed = false;
        for _ in 0..5 {
            match tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
                Ok(Ok(BackgroundEvent::Completed { .. })) => {
                    saw_completed = true;
                    break;
                }
                Ok(Ok(_)) => {}
                _ => break,
            }
        }
        assert!(!saw_completed, "killed task must not emit Completed");
        assert!(
            seen.lock().unwrap().is_empty(),
            "killed task must not steer a completion summary"
        );
    }

    #[tokio::test]
    async fn status_is_non_consuming() {
        let registry = BackgroundRegistry::new();
        let id = registry.spawn("echo data", Path::new("/tmp")).unwrap();
        // Wait until the output lands, then a poll advances the read cursor.
        let mut polled = pi::PollResult {
            new_output: String::new(),
            is_running: true,
            exit_code: None,
            total_bytes: 0,
        };
        for _ in 0..50 {
            polled = registry.poll(&id).await.unwrap();
            if polled.new_output.contains("data") || !polled.is_running {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(polled.new_output.contains("data"));
        // Status reads the whole tail regardless of the cursor position.
        let status = registry.status(&id, 1024).unwrap();
        assert!(
            status.output_tail.contains("data"),
            "status ignores the cursor"
        );
    }
}
