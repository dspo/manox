//! Async Sailor dispatch — runs a dispatched Sailor subagent in the
//! background and notifies the Captain on completion.
//!
//! `Agent(Sailor)` is synchronous in the kernel (`SubagentTool::execute`
//! awaits the child session). The host wraps it: a non-read-only dispatch is
//! spawned in a tokio task, registered as a `TaskKind::Sailor` background
//! task, and its final text is delivered to the Captain via
//! `BackendNotice::SailorCompleted` — the facade injects that as a peer
//! message and fires a turn, so the Captain reliably observes the result
//! without polling. A `BackgroundTaskUpdated` snapshot mirrors the task into
//! the legacy registry (emitted at dispatch as Running and again at
//! settlement) so UI cards / `snapshots_for_thread` share one id space with
//! monitors and background bash.
//!
//! Cancel today: a parent-turn abort (the dispatch's `parent_signal`)
//! cancels the child token via a watcher that exits when the run settles (no
//! leak); the Running card's Stop button calls `background_task::stop`; and
//! the model-facing `TaskStop` stops a Sailor via the `LegacyAwareTaskStop`
//! host wrapper (which calls `background_task::stop` for legacy-registry
//! ids). `BashOutput` does NOT yet recognize Sailor ids (progress-pull
//! follow-up). The fourth path is natural completion; thread delete cancels
//! via `cancel_all_for_thread` (run_actor, before `cleanup_thread`).

use std::sync::Arc;

use pi::tool::{AgentTool, AgentToolResult, ToolContext, ToolError};
use pi::types::ContentBlock;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::background_task::{self, TaskKind, TaskStatus};
use crate::thread::ThreadEvent;
use crate::thread_engine::BackendNotice;

/// Host-side async dispatcher for general-purpose (write+bash) subagents.
/// Read-only subagents (Explore) stay synchronous — they are cheap and need
/// no progress reporting. The manager owns an `Arc<SubagentTool>` (fully
/// configured: model, runtime, provider registry, tool snapshot) plus an
/// owned [`ToolContext`] so the spawned task can run the kernel's
/// `execute_inner` (including `isolation: "worktree"`) without borrowing the
/// caller's context across the spawn boundary.
pub struct SailorManager {
    inner: Arc<dyn AgentTool>,
    ctx: Arc<dyn ToolContext>,
    notice_tx: mpsc::UnboundedSender<BackendNotice>,
    owner_thread_id: String,
}

impl SailorManager {
    pub fn new(
        inner: Arc<dyn AgentTool>,
        ctx: Arc<dyn ToolContext>,
        notice_tx: mpsc::UnboundedSender<BackendNotice>,
        owner_thread_id: String,
    ) -> Self {
        Self {
            inner,
            ctx,
            notice_tx,
            owner_thread_id,
        }
    }

    /// Dispatch a Sailor asynchronously. Returns immediately with a JSON
    /// payload naming the `sailor_id`; the child session runs in the
    /// background and its final text is delivered via
    /// [`BackendNotice::SailorCompleted`] when it settles (a parent-turn
    /// abort settles the task silently — no revival turn).
    pub async fn dispatch(
        &self,
        subagent_type: String,
        prompt: String,
        isolation: Option<String>,
        parent_signal: CancellationToken,
    ) -> Result<AgentToolResult, ToolError> {
        let task_cancel = CancellationToken::new();
        let description = first_line(&prompt).unwrap_or_else(|| format!("Sailor {subagent_type}"));
        let params = serde_json::json!({
            "subagent_type": subagent_type,
            "prompt": prompt,
            "isolation": isolation,
        });
        let (task_id, task) = background_task::register(
            TaskKind::Sailor,
            self.owner_thread_id.clone(),
            description,
            task_cancel.clone(),
        );
        let sailor_id = task_id.0.clone();
        // Emit a Running snapshot immediately so the card surfaces during
        // the run, not only at settlement.
        let _ = self.notice_tx.send(BackendNotice::Event(Box::new(
            ThreadEvent::BackgroundTaskUpdated {
                snapshot: task.snapshot(&task_id),
            },
        )));

        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
        let inner = self.inner.clone();
        let ctx = self.ctx.clone();
        let notice_tx = self.notice_tx.clone();
        let sailor_label = format!("Sailor {sailor_id}");

        // Watcher: a parent-turn abort cancels the child token; the watcher
        // exits when the run settles so it never leaks (S3).
        let parent = parent_signal.clone();
        let child = task_cancel.clone();
        crate::runtime::handle().spawn(async move {
            tokio::select! {
                _ = parent.cancelled() => { child.cancel(); }
                _ = done_rx => {}
            }
        });

        crate::runtime::handle().spawn(async move {
            let res = inner.execute("sailor", params, task_cancel, &*ctx).await;
            // Release the watcher whether the run succeeded, failed, or was
            // aborted — the token is settled either way.
            let _ = done_tx.send(());
            match res {
                Ok(result) => {
                    let content = extract_text(&result);
                    task.set_terminal_status(TaskStatus::Completed);
                    let _ = notice_tx.send(BackendNotice::Event(Box::new(
                        ThreadEvent::BackgroundTaskUpdated {
                            snapshot: task.snapshot(&task_id),
                        },
                    )));
                    // Only revive the Captain with real content; an empty
                    // summary is a no-op turn.
                    if !content.is_empty() {
                        let _ = notice_tx.send(BackendNotice::SailorCompleted {
                            sailor_id: sailor_label,
                            content,
                        });
                    }
                }
                Err(ToolError::Aborted) => {
                    // Parent/TaskStop abort: settle the card silently — no
                    // SailorCompleted, no revival turn (B5).
                    task.set_terminal_status(TaskStatus::Stopped);
                    let _ = notice_tx.send(BackendNotice::Event(Box::new(
                        ThreadEvent::BackgroundTaskUpdated {
                            snapshot: task.snapshot(&task_id),
                        },
                    )));
                }
                Err(e) => {
                    task.set_terminal_status(TaskStatus::Failed);
                    let _ = notice_tx.send(BackendNotice::Event(Box::new(
                        ThreadEvent::BackgroundTaskUpdated {
                            snapshot: task.snapshot(&task_id),
                        },
                    )));
                    let _ = notice_tx.send(BackendNotice::SailorCompleted {
                        sailor_id: sailor_label,
                        content: format!("Sailor failed before producing output: {e}"),
                    });
                }
            }
        });

        Ok(AgentToolResult::text(
            serde_json::json!({
                "sailor_id": sailor_id,
                "status": "dispatched",
                "isolation": isolation.unwrap_or_else(|| "none".to_string()),
            })
            .to_string(),
        ))
    }
}

/// The first non-empty line of a prompt, for the background-task card title.
fn first_line(prompt: &str) -> Option<String> {
    prompt
        .lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .map(str::to_string)
}

/// Concatenate the text blocks of a tool result into a single string — the
/// Sailor's final summary the Captain sees on completion.
fn extract_text(result: &AgentToolResult) -> String {
    let mut out = String::new();
    for block in &result.content {
        if let ContentBlock::Text { text, .. } = block {
            out.push_str(text);
            out.push('\n');
        }
    }
    out.trim().to_string()
}
