// Background orchestration — the runtime half that binds background tasks
// to an agent session.
//
// The execution engine (`BackgroundRegistry`) spawns, polls, and kills; this
// module closes the loop with the agent runtime:
//
//   1. task → model: a completed task steers a summary into the agent's
//      context at the next tool-call boundary, shaped by the task's head/tail
//      line preference (the model can still fetch the full output via
//      `BashOutput`, whose read cursor is untouched); every stdout line is
//      emitted to the observer as an `Output` lifecycle event.
//   2. run → task: an aborted run kills this manager's tasks with cause
//      `RunAbort`; a settled run keeps them so a long task survives across
//      turns. Session teardown goes through the host task center, whose stop
//      hook lands here as `kill` — first-wins settlement keeps the host's
//      terminal status.
//   3. task → host: the [`TaskObserver`] receives `Spawned` / `Output` /
//      `Settled` emissions directly — one lifecycle vocabulary, no mirror.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use crate::core::BackgroundTaskRegistry;
use crate::core::coding_agent::AgentSession;
use crate::core::harness::{HarnessListener, HarnessSubscription};
use crate::core::types::AgentMessage;
use crate::ext::tasks::{
    Settlement, SettlementCause, SettlementKind, StopHandle, TaskFamily, TaskObserver,
};

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
    fn apply(self, text: &str) -> String {
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

/// Orchestrates background tasks against one agent session.
///
/// Not `Clone`-cheap by design: one manager per session. `spawn` goes
/// through the registry but also registers the task with this run, watches
/// for completion, steers a summary into the bound session, and emits
/// lifecycle events to the observer. Without a bound steerer the task still
/// runs and emits — only the model injection is skipped.
pub struct BackgroundManager {
    pub(crate) registry: Arc<BackgroundRegistry>,
    /// How a completion summary reaches the model (`HarnessHandle::steer`).
    /// Guarded so a task spawned before `attach` can still steer once a
    /// session is bound.
    steerer: Arc<Mutex<Option<Steerer>>>,
    /// Tokio handle captured at construction, used to spawn the abort
    /// cleanup from the listener (which may run on any thread).
    runtime: Option<tokio::runtime::Handle>,
    /// Tasks owned by this manager awaiting settlement, with their
    /// output-shaping preference. Removed at settlement time: by the
    /// watcher on a natural completion, by the kill pass (`kill_all`) on
    /// an abort/teardown, and by the watcher's killed branch after a
    /// single-task kill. A task absent here is settled or dead.
    tasks: Arc<Mutex<HashMap<crate::core::TaskId, OutputShape>>>,
    /// Cause recorded by kill sites (user stop / run abort / teardown)
    /// before killing; the watcher settles killed tasks with it instead of
    /// a natural completion.
    killed: Arc<Mutex<HashMap<crate::core::TaskId, SettlementCause>>>,
    observer: Mutex<Option<Arc<dyn TaskObserver>>>,
    self_weak: Weak<BackgroundManager>,
    /// Lifecycle subscription; dropped with the manager. Guarded so the
    /// manager can be shared behind an `Arc` (a bash tool holds it) while
    /// still attaching to a session.
    _lifecycle: Arc<Mutex<Option<HarnessSubscription>>>,
}

impl BackgroundManager {
    pub fn new(registry: Arc<BackgroundRegistry>) -> Arc<Self> {
        Arc::new_cyclic(|weak| BackgroundManager {
            registry,
            steerer: Arc::new(Mutex::new(None)),
            runtime: tokio::runtime::Handle::try_current().ok(),
            tasks: Arc::new(Mutex::new(HashMap::new())),
            killed: Arc::new(Mutex::new(HashMap::new())),
            observer: Mutex::new(None),
            self_weak: weak.clone(),
            _lifecycle: Arc::new(Mutex::new(None)),
        })
    }

    /// Bind the lifecycle sink. Spawns before this call emit nothing; the
    /// host binds it during assembly, before any tool can run.
    pub fn set_observer(&self, observer: Arc<dyn TaskObserver>) {
        *self.observer.lock().expect("observer lock poisoned") = Some(observer);
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
        let killed = Arc::clone(&self.killed);
        // Refresh the handle in case construction happened outside a runtime;
        // the listener itself may run on any thread.
        let runtime = self
            .runtime
            .clone()
            .or_else(|| tokio::runtime::Handle::try_current().ok());
        let listener: HarnessListener = Arc::new(move |event| {
            if matches!(event, crate::core::harness::HarnessEvent::Abort { .. }) {
                let registry = Arc::clone(&registry);
                let tasks = Arc::clone(&tasks);
                let killed = Arc::clone(&killed);
                match &runtime {
                    Some(runtime) => {
                        runtime.spawn(async move {
                            kill_all_tasks(&registry, &tasks, &killed, SettlementCause::RunAbort)
                                .await;
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
    /// Escalated (unconfined) path.
    pub fn spawn(
        &self,
        command: &str,
        cwd: &std::path::Path,
        shape: OutputShape,
    ) -> Result<crate::core::TaskId, crate::core::TaskError> {
        self.spawn_impl(command, cwd, shape, false)
    }

    /// Spawn a background task through the registry's sandbox wrapper (when
    /// configured), else bare. Used for non-escalated background tasks: the
    /// seatbelt confines writes + network like a foreground sandboxed call.
    pub fn spawn_sandboxed(
        &self,
        command: &str,
        cwd: &std::path::Path,
        shape: OutputShape,
    ) -> Result<crate::core::TaskId, crate::core::TaskError> {
        self.spawn_impl(command, cwd, shape, true)
    }

    /// Common spawn core: line-event spawn, lifecycle registration, and the
    /// completion watcher.
    fn spawn_impl(
        &self,
        command: &str,
        cwd: &std::path::Path,
        shape: OutputShape,
        sandboxed: bool,
    ) -> Result<crate::core::TaskId, crate::core::TaskError> {
        let observer = self
            .observer
            .lock()
            .expect("observer lock poisoned")
            .clone();
        let id = if sandboxed {
            self.registry.spawn_with_line_events(
                command,
                cwd,
                Box::new({
                    let observer = observer.clone();
                    move |id, line| {
                        if let Some(obs) = &observer {
                            obs.on_output(&id.0, line);
                        }
                    }
                }),
                Box::new(|_, _| {}),
            )?
        } else {
            self.registry.spawn_escalated_with_line_events(
                command,
                cwd,
                Box::new({
                    let observer = observer.clone();
                    move |id, line| {
                        if let Some(obs) = &observer {
                            obs.on_output(&id.0, line);
                        }
                    }
                }),
                Box::new(|_, _| {}),
            )?
        };
        self.tasks
            .lock()
            .expect("tasks lock poisoned")
            .insert(id.clone(), shape);
        if let Some(obs) = &observer {
            obs.on_spawned(
                &id.0,
                TaskFamily::BackgroundBash,
                &format!("background bash: {command}"),
                self.stop_handle(&id.0),
            );
        }

        let registry = Arc::clone(&self.registry);
        let steerer = Arc::clone(&self.steerer);
        let tasks = Arc::clone(&self.tasks);
        let killed = Arc::clone(&self.killed);
        let tid = id.clone();
        tokio::spawn(async move {
            // Event-driven: the drain task notifies the moment the exit is
            // recorded, so no polling interval delays the completion.
            if registry.wait_exit(&tid).await.is_err() {
                if let Some(obs) = &observer {
                    obs.on_settled(
                        &tid.0,
                        &Settlement::new(SettlementKind::Failed, SettlementCause::Natural)
                            .with_failure_summary(Some("task disappeared before exit".into())),
                    );
                }
                return;
            }
            // First-wins witness: a kill site (user stop / run abort /
            // teardown) drained the task's entry and recorded the cause, so
            // the watcher settles with that cause and must not steer a
            // completion summary into the session.
            if let Some(cause) = killed.lock().expect("killed lock poisoned").remove(&tid) {
                tasks.lock().expect("tasks lock poisoned").remove(&tid);
                if let Some(obs) = &observer {
                    obs.on_settled(&tid.0, &Settlement::new(SettlementKind::Stopped, cause));
                }
                return;
            }
            let status = match registry.status(&tid, status_tail_bytes(&shape)) {
                Ok(status) => status,
                Err(e) => {
                    if let Some(obs) = &observer {
                        obs.on_settled(
                            &tid.0,
                            &Settlement::new(SettlementKind::Failed, SettlementCause::Natural)
                                .with_failure_summary(Some(e.to_string())),
                        );
                    }
                    return;
                }
            };
            if let Some(shape) = tasks.lock().expect("tasks lock poisoned").remove(&tid) {
                let steerer = steerer.lock().expect("steerer lock poisoned").clone();
                let settlement =
                    Settlement::new(SettlementKind::Completed, SettlementCause::Natural)
                        .with_exit_code(status.exit_code.flatten());
                if let Some(steer) = steerer {
                    steer(AgentMessage::user(format_summary(&tid, &status, shape)));
                }
                if let Some(obs) = &observer {
                    obs.on_settled(&tid.0, &settlement);
                }
            }
        });
        Ok(id)
    }

    /// Cancel every task this manager owns with an explicit cause.
    pub async fn kill_all(&self, cause: SettlementCause) {
        kill_all_tasks(&self.registry, &self.tasks, &self.killed, cause).await;
    }

    /// Kill one task synchronously (user-facing stop). The completion watcher
    /// settles the task with the recorded cause; first-wins keeps any
    /// terminal status the host task center already pushed.
    pub fn kill(&self, id: &crate::core::TaskId) {
        self.kill_with_cause(id, SettlementCause::UserStop);
    }

    /// Kill one task with the stopping side's explicit cause. The cause map
    /// is first-wins, matching `MonitorManager`: a stop racing a run abort
    /// keeps the first label.
    pub fn kill_with_cause(&self, id: &crate::core::TaskId, cause: SettlementCause) {
        self.killed
            .lock()
            .expect("killed lock poisoned")
            .entry(id.clone())
            .or_insert(cause);
        let _ = self.registry.kill_sync(id);
    }

    /// Poll one task's status, with a bounded tail of its output.
    pub fn status(
        &self,
        id: &crate::core::TaskId,
        tail_bytes: usize,
    ) -> Result<TaskStatusInfo, crate::core::TaskError> {
        self.registry.status(id, tail_bytes)
    }

    /// The kill path for one task id, for the lifecycle `Spawned` emission:
    /// forwards the stopping side's cause so the kill site records it.
    fn stop_handle(&self, id: &str) -> StopHandle {
        let weak = self.self_weak.clone();
        let tid = crate::core::TaskId(id.to_string());
        Arc::new(move |cause| {
            if let Some(manager) = weak.upgrade() {
                manager.kill_with_cause(&tid, cause);
            }
        })
    }

    /// Test-only injection point for a recording steerer without a session.
    #[cfg(test)]
    pub(crate) fn set_test_steerer(&self, f: impl Fn(AgentMessage) + Send + Sync + 'static) {
        *self.steerer.lock().expect("steerer lock poisoned") = Some(Arc::new(f));
    }

    /// Test-only injection point for a recording observer.
    #[cfg(test)]
    pub(crate) fn set_test_observer(&self, observer: Arc<dyn TaskObserver>) {
        self.set_observer(observer);
    }
}

fn format_summary(id: &crate::core::TaskId, status: &TaskStatusInfo, shape: OutputShape) -> String {
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
    tasks: &Mutex<HashMap<crate::core::TaskId, OutputShape>>,
    killed: &Mutex<HashMap<crate::core::TaskId, SettlementCause>>,
    cause: SettlementCause,
) {
    // Drain: a killed task's bookkeeping leaves the map here, so a later
    // kill pass never re-touches (and re-GC-refreshes) historical ids.
    let drained: Vec<crate::core::TaskId> = tasks
        .lock()
        .expect("tasks lock poisoned")
        .drain()
        .map(|(id, _)| id)
        .collect();
    for id in drained {
        killed
            .lock()
            .expect("killed lock poisoned")
            .entry(id.clone())
            .or_insert(cause);
        let _ = registry.kill(&id).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bash::test_helpers::wait_for_steered;
    use crate::core::types::ContentBlock;
    use crate::ext::tasks::TaskLifecycle;
    use std::path::Path;
    use std::time::Duration;

    fn new_manager() -> Arc<BackgroundManager> {
        BackgroundManager::new(Arc::new(BackgroundRegistry::new()))
    }

    #[test]
    fn summary_reports_id_and_exit_code() {
        let status = TaskStatusInfo {
            is_running: false,
            exit_code: Some(Some(3)),
            output_tail: "boom".into(),
        };
        let summary = format_summary(
            &crate::core::TaskId("bg_1".into()),
            &status,
            OutputShape::default(),
        );
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
        let summary = format_summary(
            &crate::core::TaskId("bg_1".into()),
            &status,
            OutputShape::default(),
        );
        assert!(summary.contains("terminated by signal"));
        assert!(!summary.contains("Recent output"));
    }

    /// A spawn emits Spawned + Output lines + exactly one Settled, and the
    /// completion summary reaches the steerer.
    #[tokio::test]
    async fn spawn_emits_lifecycle_and_completion_steers() {
        let manager = new_manager();
        let observer = crate::ext::tasks::tests::RecordingObserver::new();
        manager.set_test_observer(Arc::clone(&observer) as Arc<dyn TaskObserver>);
        let seen: Arc<Mutex<Vec<AgentMessage>>> = Arc::new(Mutex::new(Vec::new()));
        let seen2 = Arc::clone(&seen);
        manager.set_test_steerer(move |m| seen2.lock().unwrap().push(m));

        let id = manager
            .spawn(
                "echo hello; sleep 0.1",
                Path::new("/tmp"),
                OutputShape::default(),
            )
            .unwrap();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let settled = observer
                .events
                .lock()
                .unwrap()
                .iter()
                .any(|ev| matches!(ev, TaskLifecycle::Settled { id: sid, .. } if sid == &id.0));
            if settled || tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let events = observer.events.lock().unwrap().clone();
        assert!(
            events
                .iter()
                .any(|ev| matches!(ev, TaskLifecycle::Spawned { .. })),
            "spawned event observed: {events:?}"
        );
        assert!(
            events.iter().any(
                |ev| matches!(ev, TaskLifecycle::Output { line, .. } if line.contains("hello"))
            ),
            "output lines observed: {events:?}"
        );
        let settled: Vec<_> = events
            .iter()
            .filter_map(|ev| match ev {
                TaskLifecycle::Settled { settlement, .. } => Some(settlement.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(settled.len(), 1, "exactly one settlement: {settled:?}");
        assert!(matches!(settled[0].kind, SettlementKind::Completed));

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
    async fn kill_all_cancels_tasks_and_settles_killed() {
        let manager = new_manager();
        let observer = crate::ext::tasks::tests::RecordingObserver::new();
        manager.set_test_observer(Arc::clone(&observer) as Arc<dyn TaskObserver>);
        let id = manager
            .spawn("sleep 30", Path::new("/tmp"), OutputShape::default())
            .unwrap();
        assert!(manager.registry.status(&id, 0).unwrap().is_running);

        manager.kill_all(SettlementCause::Teardown).await;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let settled = observer
                .events
                .lock()
                .unwrap()
                .iter()
                .any(|ev| matches!(ev, TaskLifecycle::Settled { id: sid, .. } if sid == &id.0));
            if settled || tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let settled: Vec<_> = observer
            .events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|ev| match ev {
                TaskLifecycle::Settled { settlement, .. } => Some(settlement.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(settled.len(), 1, "exactly one settlement: {settled:?}");
        assert_eq!(settled[0].cause, SettlementCause::Teardown);
        assert!(matches!(settled[0].kind, SettlementKind::Stopped));
        // The kill path releases its bookkeeping: neither the task table
        // nor the cause map may retain the killed id (a later kill pass
        // would otherwise re-touch — and re-GC-refresh — historical ids).
        assert!(manager.tasks.lock().unwrap().is_empty());
        assert!(manager.killed.lock().unwrap().is_empty());
    }

    /// Regression: a task killed via `kill_all` must neither steer a
    /// completion summary nor settle Completed — the cause recorded by the
    /// kill site routes the settlement to Stopped.
    #[tokio::test]
    async fn killed_task_does_not_steer_or_complete() {
        let manager = new_manager();
        let observer = crate::ext::tasks::tests::RecordingObserver::new();
        manager.set_test_observer(Arc::clone(&observer) as Arc<dyn TaskObserver>);
        let seen: Arc<Mutex<Vec<AgentMessage>>> = Arc::new(Mutex::new(Vec::new()));
        let seen2 = Arc::clone(&seen);
        manager.set_test_steerer(move |m| seen2.lock().unwrap().push(m));

        let _id = manager
            .spawn("sleep 30", Path::new("/tmp"), OutputShape::default())
            .unwrap();
        manager.kill_all(SettlementCause::RunAbort).await;
        // Give the watcher a poll cycle to observe the exit.
        tokio::time::sleep(Duration::from_millis(300)).await;

        let settled: Vec<_> = observer
            .events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|ev| match ev {
                TaskLifecycle::Settled { settlement, .. } => Some(settlement.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(settled.len(), 1, "exactly one settlement: {settled:?}");
        assert_eq!(settled[0].cause, SettlementCause::RunAbort);
        assert!(matches!(settled[0].kind, SettlementKind::Stopped));
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
        let mut polled = crate::core::PollResult {
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
            &crate::core::TaskId("bg_1".into()),
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
            &crate::core::TaskId("bg_1".into()),
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
            &crate::core::TaskId("bg_1".into()),
            &status,
            OutputShape {
                head_lines: Some(0),
                tail_lines: None,
            },
        );
        assert!(!summary.contains("Recent output"), "empty shape: {summary}");
        // tail_lines: Some(0) also drops every line and omits the section.
        let summary = format_summary(
            &crate::core::TaskId("bg_1".into()),
            &status,
            OutputShape {
                head_lines: None,
                tail_lines: Some(0),
            },
        );
        assert!(
            !summary.contains("Recent output"),
            "empty tail shape: {summary}"
        );
    }

    #[tokio::test]
    async fn spawn_steers_shaped_summary() {
        let manager = new_manager();
        let observer = crate::ext::tasks::tests::RecordingObserver::new();
        manager.set_test_observer(Arc::clone(&observer) as Arc<dyn TaskObserver>);
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
}
