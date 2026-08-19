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
//! the legacy registry so UI cards / `snapshots_for_thread` share one id
//! space with monitors and background bash. The Captain cancels a running
//! Sailor via `TaskStop` (the task's cancel token interrupts the child
//! session); a parent-turn abort also propagates to the child token.

use std::sync::Arc;

use pi::tool::{AgentTool, AgentToolResult, ToolContext, ToolError};
use pi::types::ContentBlock;
use pi_extensions::agents::SubagentTool;
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
    inner: Arc<SubagentTool>,
    ctx: Arc<dyn ToolContext>,
    notice_tx: mpsc::UnboundedSender<BackendNotice>,
    owner_thread_id: String,
}

impl SailorManager {
    pub fn new(
        inner: Arc<SubagentTool>,
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
    /// [`BackendNotice::SailorCompleted`] when it settles.
    pub async fn dispatch(
        &self,
        subagent_type: String,
        prompt: String,
        isolation: Option<String>,
        parent_signal: CancellationToken,
    ) -> Result<AgentToolResult, ToolError> {
        let task_cancel = CancellationToken::new();
        let params = serde_json::json!({
            "subagent_type": subagent_type,
            "prompt": prompt,
            "isolation": isolation,
        });
        let (task_id, task) = background_task::register(
            TaskKind::Sailor,
            self.owner_thread_id.clone(),
            format!("Sailor {subagent_type}"),
            task_cancel.clone(),
        );
        let sailor_id = task_id.0.clone();
        let sailor_id_return = sailor_id.clone();
        let inner = self.inner.clone();
        let ctx = self.ctx.clone();
        let notice_tx = self.notice_tx.clone();

        // A parent-turn abort cancels the child token; TaskStop also cancels
        // it directly. One tiny watcher bridges the two.
        let parent = parent_signal.clone();
        let child = task_cancel.clone();
        crate::runtime::handle().spawn(async move {
            parent.cancelled().await;
            child.cancel();
        });

        crate::runtime::handle().spawn(async move {
            let res = inner.execute("sailor", params, task_cancel, &*ctx).await;
            let (content, terminal) = match res {
                Ok(result) => (extract_text(&result), true),
                Err(e) => (format!("Sailor failed before producing output: {e}"), false),
            };
            task.set_terminal_status(if terminal {
                TaskStatus::Completed
            } else {
                TaskStatus::Failed
            });
            let snapshot = task.snapshot(&task_id);
            let _ = notice_tx.send(BackendNotice::Event(Box::new(
                ThreadEvent::BackgroundTaskUpdated { snapshot },
            )));
            let _ = notice_tx.send(BackendNotice::SailorCompleted { sailor_id, content });
        });

        Ok(AgentToolResult::text(format!(
            "{{\"sailor_id\":\"{sailor_id_return}\",\"status\":\"dispatched\",\"isolation\":\"{}\"}}",
            isolation.as_deref().unwrap_or("none")
        )))
    }
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
