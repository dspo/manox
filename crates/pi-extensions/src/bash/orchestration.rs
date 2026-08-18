// Background orchestration — the runtime half that binds background tasks
// to an agent session.
//
// The execution engine (`BackgroundRegistry`) spawns, polls, and kills; this
// module closes the loop with the agent runtime:
//
//   1. task → model: a completed task steers a summary into the agent's
//      context at the next tool-call boundary, shaped by the task's head/tail
//      line preference (the model can still fetch the full output via
//      `bash_output`, whose read cursor is untouched).
//   2. run → task: an aborted run kills its background tasks; a settled run
//      keeps them so a long task survives across turns. The caller kills
//      everything on session teardown via [`BackgroundManager::kill_all`].
//   3. task → events: a `BackgroundEvent` stream for UI / audit consumers.

use std::collections::HashMap;
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

/// Line-based output-shaping preference for a background task's completion
/// summary; mirrors the foreground `head_lines`/`tail_lines` semantics.
#[derive(Debug, Clone, Copy, Default)]
pub struct OutputShape {
    pub head_lines: Option<usize>,
    pub tail_lines: Option<usize>,
}

impl OutputShape {
    /// Apply the line preference to a text window; no preference returns the
    /// window unchanged.
    pub fn apply(self, text: &str) -> String {
        super::select_lines(text, self.head_lines, self.tail_lines)
    }
}

/// Byte window fetched for the completion summary. With a head/tail line
/// preference the whole retained ring is fetched so line shaping sees the
/// true head and tail of the available output; otherwise the fixed byte
/// tail keeps the summary cheap.
fn status_tail_bytes(shape: &OutputShape) -> usize {
    if shape.head_lines.is_some() || shape.tail_lines.is_some() {
        usize::MAX
    } else {
        SUMMARY_TAIL_BYTES
    }
}

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
    /// Tasks owned by this manager, with their output-shaping preference;
    /// killed together on abort / teardown.
    tasks: Arc<Mutex<HashMap<pi::TaskId, OutputShape>>>,
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
            tasks: Arc::new(Mutex::new(HashMap::new())),
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
    pub fn spawn(
        &self,
        command: &str,
        cwd: &std::path::Path,
        shape: OutputShape,
    ) -> Result<pi::TaskId, pi::TaskError> {
        // Bare spawn: used for escalated background tasks (no confinement).
        let id = self.registry.spawn(command, cwd)?;
        self.track_and_observe(id, command, shape)
    }

    /// Spawn a background task through the registry's sandbox wrapper (when
    /// configured), else bare. Used for non-escalated background tasks: the
    /// seatbelt confines writes + network like a foreground sandboxed call.
    pub fn spawn_sandboxed(
        &self,
        command: &str,
        cwd: &std::path::Path,
        shape: OutputShape,
    ) -> Result<pi::TaskId, pi::TaskError> {
        let id = self.registry.spawn_sandboxed(command, cwd)?;
        self.track_and_observe(id, command, shape)
    }

    /// Register the task with this run, emit the spawned event, and arm the
    /// completion observer (steer a summary / emit Completed exactly once).
    fn track_and_observe(
        &self,
        id: pi::TaskId,
        command: &str,
        shape: OutputShape,
    ) -> Result<pi::TaskId, pi::TaskError> {
        self.tasks
            .lock()
            .expect("tasks lock poisoned")
            .insert(id.clone(), shape);
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
            let status = match registry.status(&tid, status_tail_bytes(&shape)) {
                Ok(status) => status,
                Err(e) => {
                    let _ = event_tx.send(BackgroundEvent::Failed {
                        id: tid.clone(),
                        reason: e.to_string(),
                    });
                    return;
                }
            };
            // Exactly-once: `kill_all` (abort / teardown) drains the map
            // first, so a killed task must not steer a "completed" summary
            // into the session.
            if let Some(shape) = tasks.lock().expect("tasks lock poisoned").remove(&tid) {
                let steerer = steerer.lock().expect("steerer lock poisoned").clone();
                finish_task(&tid, &status, shape, steerer.as_ref(), &event_tx);
            }
        });
        Ok(id)
    }

    /// Cancel every task this manager owns and emit `Killed` for each.
    pub async fn kill_all(&self) {
        kill_all_tasks(&self.registry, &self.tasks, &self.event_tx).await;
    }

    /// Kill one task synchronously (user-facing stop). The task's exit path
    /// emits `Killed` exactly once.
    pub fn kill(&self, id: &pi::TaskId) {
        let _ = self.registry.kill_sync(id);
    }

    /// Poll one task's status, with a bounded tail of its output.
    pub fn status(
        &self,
        id: &pi::TaskId,
        tail_bytes: usize,
    ) -> Result<TaskStatusInfo, pi::TaskError> {
        self.registry.status(id, tail_bytes)
    }

    /// Subscribe to background events; dropping the receiver unsubscribes.
    pub fn subscribe(&self) -> broadcast::Receiver<BackgroundEvent> {
        self.event_tx.subscribe()
    }

    /// Test-only injection point for a recording steerer without a session.
    #[cfg(test)]
    pub(crate) fn set_test_steerer(&self, f: impl Fn(AgentMessage) + Send + Sync + 'static) {
        *self.steerer.lock().expect("steerer lock poisoned") = Some(Arc::new(f));
    }
}

/// Steer a completion summary into the bound session and emit the event.
fn finish_task(
    id: &pi::TaskId,
    status: &TaskStatusInfo,
    shape: OutputShape,
    steerer: Option<&Steerer>,
    event_tx: &broadcast::Sender<BackgroundEvent>,
) {
    if let Some(steer) = steerer {
        steer(AgentMessage::user(format_summary(id, status, shape)));
    }
    let _ = event_tx.send(BackgroundEvent::Completed {
        id: id.clone(),
        exit_code: status.exit_code.flatten(),
    });
}

/// Build the model-facing completion notice.
fn format_summary(id: &pi::TaskId, status: &TaskStatusInfo, shape: OutputShape) -> String {
    let code = match status.exit_code {
        Some(Some(code)) => format!("exit code {code}"),
        Some(None) => "terminated by signal".to_string(),
        None => "finished".to_string(),
    };
    let tail = {
        let shaped = shape.apply(&status.output_tail);
        if shaped.trim().is_empty() {
            String::new()
        } else {
            format!("\n\nRecent output:\n{}", shaped.trim_end())
        }
    };
    format!(
        "Background task `{id}` completed ({code}).{tail}\n\nUse `BashOutput` (shell_id: \"{id}\") for the full output."
    )
}

async fn kill_all_tasks(
    registry: &BackgroundRegistry,
    tasks: &Mutex<HashMap<pi::TaskId, OutputShape>>,
    event_tx: &broadcast::Sender<BackgroundEvent>,
) {
    let tasks: Vec<(pi::TaskId, OutputShape)> =
        tasks.lock().expect("tasks lock poisoned").drain().collect();
    for (id, _shape) in tasks {
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
        let summary = format_summary(&pi::TaskId("bg_1".into()), &status, OutputShape::default());
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
        let summary = format_summary(&pi::TaskId("bg_1".into()), &status, OutputShape::default());
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
            .spawn(
                "echo hello; sleep 0.1",
                Path::new("/tmp"),
                OutputShape::default(),
            )
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
        let id = manager
            .spawn("sleep 30", Path::new("/tmp"), OutputShape::default())
            .unwrap();
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
        let _id = manager
            .spawn("sleep 30", Path::new("/tmp"), OutputShape::default())
            .unwrap();
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

    #[test]
    fn summary_shapes_tail_by_line_preference() {
        let ten_lines: String = (1..=10).map(|i| format!("line {i}\n")).collect();
        let status = TaskStatusInfo {
            is_running: false,
            exit_code: Some(Some(0)),
            output_tail: ten_lines,
        };
        // tail_lines keeps only the last N lines.
        let summary = format_summary(
            &pi::TaskId("bg_1".into()),
            &status,
            OutputShape {
                head_lines: None,
                tail_lines: Some(3),
            },
        );
        let lines = recent_output_lines(&summary);
        assert_eq!(
            lines,
            vec!["line 8", "line 9", "line 10"],
            "tail lines: {lines:?}"
        );
        assert!(!lines.contains(&"line 1"), "head dropped: {lines:?}");
        // head + tail insert the "..." separator, like the foreground path.
        let summary = format_summary(
            &pi::TaskId("bg_1".into()),
            &status,
            OutputShape {
                head_lines: Some(2),
                tail_lines: Some(2),
            },
        );
        let lines = recent_output_lines(&summary);
        assert_eq!(
            lines,
            vec!["line 1", "line 2", "...", "line 9", "line 10"],
            "head + tail lines: {lines:?}"
        );
        assert!(!lines.contains(&"line 5"), "middle dropped: {lines:?}");
        // A shape that drops every line omits the "Recent output" section.
        let summary = format_summary(
            &pi::TaskId("bg_1".into()),
            &status,
            OutputShape {
                head_lines: Some(0),
                tail_lines: None,
            },
        );
        assert!(!summary.contains("Recent output"), "empty shape: {summary}");
    }

    #[tokio::test]
    async fn spawn_steers_shaped_summary() {
        let manager = new_manager();
        let seen: Arc<Mutex<Vec<AgentMessage>>> = Arc::new(Mutex::new(Vec::new()));
        let seen2 = Arc::clone(&seen);
        manager.set_test_steerer(move |m| seen2.lock().unwrap().push(m));

        let tail_id = manager
            .spawn(
                "printf 'l1\\nl2\\nl3\\nl4\\nl5\\n'",
                Path::new("/tmp"),
                OutputShape {
                    head_lines: None,
                    tail_lines: Some(2),
                },
            )
            .unwrap();
        let summary = wait_for_steered(&seen, 1).await;
        assert_eq!(
            recent_output_lines(&summary),
            vec!["l4", "l5"],
            "tail lines: {summary}"
        );
        assert!(
            summary.contains(&tail_id.0),
            "summary names the task: {summary}"
        );

        let head_id = manager
            .spawn(
                "printf 'l1\\nl2\\nl3\\nl4\\nl5\\n'",
                Path::new("/tmp"),
                OutputShape {
                    head_lines: Some(2),
                    tail_lines: None,
                },
            )
            .unwrap();
        let summary = wait_for_steered(&seen, 2).await;
        assert_eq!(
            recent_output_lines(&summary),
            vec!["l1", "l2"],
            "head lines: {summary}"
        );
        assert!(
            summary.contains(&head_id.0),
            "summary names the task: {summary}"
        );
    }

    /// The output lines of a completion summary's "Recent output" section,
    /// stopping at the first blank line (the "Use `BashOutput`" trailer
    /// follows).
    fn recent_output_lines(summary: &str) -> Vec<&str> {
        summary
            .split("Recent output:\n")
            .nth(1)
            .expect("summary carries a shaped output section")
            .lines()
            .take_while(|l| !l.trim().is_empty())
            .collect()
    }

    /// Wait until `count` completion summaries reached the recording steerer
    /// and return the most recent one.
    async fn wait_for_steered(seen: &Arc<Mutex<Vec<AgentMessage>>>, count: usize) -> String {
        for _ in 0..100 {
            {
                let msgs = seen.lock().unwrap();
                if msgs.len() >= count
                    && let Some(summary) = msgs.iter().rev().find_map(|m| match m {
                        AgentMessage::User { content, .. } => {
                            content.iter().find_map(|b| match b {
                                ContentBlock::Text { text, .. } => Some(text.clone()),
                                _ => None,
                            })
                        }
                        _ => None,
                    })
                {
                    return summary;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!(
            "completion summary not steered in time; {} messages seen",
            seen.lock().unwrap().len()
        );
    }
}
