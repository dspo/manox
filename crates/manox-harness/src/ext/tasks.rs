//! Unified task lifecycle vocabulary and the observer seam.
//!
//! Every background-work producer (command/WebSocket monitors, background
//! bash, subagents) reports through one [`TaskLifecycle`] stream to one
//! [`TaskObserver`]. A settlement pairs the outcome ([`SettlementKind`]) with
//! an explicit [`SettlementCause`], so consumers no longer infer "stopped by
//! whom" from scattered suppression flags: the kill site records the cause
//! before killing, and the exit path settles exactly once with it.
//!
//! Model-facing injection (steering) stays with each producer; this module
//! only carries what UI / audit consumers and the host task center need.

use std::sync::Arc;

/// Which family of background work a task belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TaskFamily {
    MonitorCommand,
    MonitorWebSocket,
    BackgroundBash,
    Subagent,
}

/// How a task was ended, independent of the outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SettlementCause {
    /// The work ran to its own end (process exit, server close, run finish).
    Natural,
    /// The model or the user stopped it (TaskStop / UI stop).
    UserStop,
    /// The run was aborted (user Esc) and abort semantics kill this family.
    RunAbort,
    /// Session or app teardown killed it; nobody is left to read a summary.
    Teardown,
    /// A wall-clock deadline elapsed and the watchdog killed it.
    Timeout,
}

/// The outcome of a settled task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SettlementKind {
    Completed,
    Failed,
    TimedOut,
    Stopped,
}

/// One task's settlement: outcome plus the explicit cause.
#[derive(Debug, Clone)]
pub struct Settlement {
    pub kind: SettlementKind,
    pub cause: SettlementCause,
    pub exit_code: Option<i32>,
    pub failure_summary: Option<String>,
}

impl Settlement {
    pub fn new(kind: SettlementKind, cause: SettlementCause) -> Self {
        Self {
            kind,
            cause,
            exit_code: None,
            failure_summary: None,
        }
    }

    pub fn with_exit_code(mut self, exit_code: Option<i32>) -> Self {
        self.exit_code = exit_code;
        self
    }

    pub fn with_failure_summary(mut self, summary: Option<String>) -> Self {
        self.failure_summary = summary;
        self
    }
}

/// One lifecycle emission from a producer.
#[derive(Debug, Clone)]
pub enum TaskLifecycle {
    Spawned {
        id: String,
        family: TaskFamily,
        label: String,
    },
    Output {
        id: String,
        line: String,
    },
    Settled {
        id: String,
        settlement: Settlement,
    },
}

/// The kill path for one task id, minted by the producer at spawn so the
/// observer's owner can stop the work without holding manager back-references.
pub type StopHandle = Arc<dyn Fn() + Send + Sync>;

/// Host-side sink for task lifecycle emissions. Producers call it directly at
/// spawn / output / settlement time; there is no second bookkeeping layer.
pub trait TaskObserver: Send + Sync {
    fn on_spawned(&self, id: &str, family: TaskFamily, label: &str, stop: StopHandle);
    fn on_output(&self, id: &str, line: String);
    fn on_settled(&self, id: &str, settlement: &Settlement);
}

/// A no-op observer for producers constructed before a host is bound; also
/// useful in tests that only exercise steering.
#[derive(Debug, Default, Clone, Copy)]
pub struct NopObserver;

impl TaskObserver for NopObserver {
    fn on_spawned(&self, _id: &str, _family: TaskFamily, _label: &str, _stop: StopHandle) {}
    fn on_output(&self, _id: &str, _line: String) {}
    fn on_settled(&self, _id: &str, _settlement: &Settlement) {}
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A recording observer for tests: captures lifecycle emissions.
    #[derive(Default)]
    pub struct RecordingObserver {
        pub events: Mutex<Vec<TaskLifecycle>>,
    }

    impl RecordingObserver {
        pub fn new() -> Arc<Self> {
            Arc::new(Self::default())
        }
    }

    impl TaskObserver for RecordingObserver {
        fn on_spawned(&self, id: &str, family: TaskFamily, label: &str, _stop: StopHandle) {
            self.events
                .lock()
                .expect("events lock poisoned")
                .push(TaskLifecycle::Spawned {
                    id: id.to_string(),
                    family,
                    label: label.to_string(),
                });
        }

        fn on_output(&self, id: &str, line: String) {
            self.events
                .lock()
                .expect("events lock poisoned")
                .push(TaskLifecycle::Output {
                    id: id.to_string(),
                    line,
                });
        }

        fn on_settled(&self, id: &str, settlement: &Settlement) {
            self.events
                .lock()
                .expect("events lock poisoned")
                .push(TaskLifecycle::Settled {
                    id: id.to_string(),
                    settlement: settlement.clone(),
                });
        }
    }
}
