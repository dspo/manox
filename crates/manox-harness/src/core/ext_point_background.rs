// Background task registry — the interface behind long-running shell
// commands (`Bash run_in_background`). The harness defines the seam;
// implementations live in extension crates so the core stays free of
// process-management policy.

use std::path::Path;

/// A unique identifier for a background task.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TaskId(pub String);

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// The outcome of polling a background task.
#[derive(Debug, Clone)]
pub struct PollResult {
    /// Output produced since the last poll.
    pub new_output: String,
    /// Whether the process is still running.
    pub is_running: bool,
    /// `Some(Some(code))` on clean exit, `Some(None)` when signaled; `None`
    /// while the task is still running.
    pub exit_code: Option<Option<i32>>,
    /// Cumulative bytes produced (advisory for truncation notices).
    pub total_bytes: u64,
}

/// Errors from the background task registry.
#[derive(Debug, thiserror::Error)]
pub enum TaskError {
    #[error("spawn error: {0}")]
    Spawn(String),
    #[error("task not found: {0}")]
    NotFound(String),
    #[error("{0}")]
    Other(String),
}

/// Registry of long-running processes owned by the harness session.
///
/// `spawn` returns immediately with an id; output accumulates in a ring
/// buffer and `poll` returns only the increment since the previous poll, so
/// a consumer (a poll-style tool) can follow progress without re-reading
/// history. `collect_exit` blocks a model-facing read until the task's exit
/// is recorded, so a consumer can wait for completion instead of polling.
/// `kill` terminates the process group.
#[async_trait::async_trait]
pub trait BackgroundTaskRegistry: Send + Sync {
    /// Start a command in the background and return its id immediately.
    fn spawn(&self, command: &str, cwd: &Path) -> Result<TaskId, TaskError>;

    /// Fetch output produced since the last poll, plus the task's status.
    async fn poll(&self, id: &TaskId) -> Result<PollResult, TaskError>;

    /// Terminate the task's process group.
    async fn kill(&self, id: &TaskId) -> Result<(), TaskError>;

    /// Resolve when the task's exit code has been recorded; an
    /// already-exited task resolves immediately. Unlike `poll`, this does
    /// not advance the read cursor. An exit that lands while this call is
    /// blocked is collected by it: the caller's result carries the terminal
    /// state, so no completion notice repeats it.
    async fn collect_exit(&self, id: &TaskId) -> Result<(), TaskError>;
}
