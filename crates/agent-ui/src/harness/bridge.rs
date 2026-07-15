//! tokio↔gpui bridge for the MCP server.
//!
//! The MCP stdio server runs on the tokio runtime (`manox` crate). It cannot
//! hold an `AsyncApp` (gpui's `AsyncApp` is `!Send`, Rc-backed). So MCP tool
//! calls are translated into `McpRequest` values and sent through an
//! `async_channel` to a long-lived gpui-executor task (the dispatcher) that
//! holds the `AsyncApp` + the target `Entity<Workspace>` and applies the
//! request via `cx.update`. Results come back on a `tokio::sync::oneshot`.
//! This mirrors the provider layer's `async_channel::bounded(64)` pattern
//! (`provider/anthropic.rs`).
//!
//! v1 is single-session: one Workspace per dispatcher. Multi-session (multiple
//! real windows, the true two-concurrent-turn repro over MCP) is a Phase 5
//! extension — the `cargo test` path covers the two-Thread repro today.

use std::time::Duration;

use agent::PermissionDecision;
use gpui::{App, AsyncApp, Entity};
use serde_json::{Value, json};
use tokio::sync::oneshot;

use crate::harness::Harness;
use crate::harness::types::IdleState;
use crate::workspace::Workspace;

pub type Reply = oneshot::Sender<Result<Value, String>>;

/// A single MCP tool call, enqueued from the tokio-side server and consumed by
/// the gpui-side dispatcher. Each variant carries its arguments plus the
/// `Reply` channel for the serialized result.
pub enum McpRequest {
    NewThread {
        reply: Reply,
    },
    OpenThread {
        id: String,
        reply: Reply,
    },
    ListThreads {
        reply: Reply,
    },
    SendMessage {
        text: String,
        reply: Reply,
    },
    SendCommand {
        name: String,
        args: String,
        reply: Reply,
    },
    Approve {
        decision: PermissionDecision,
        reply: Reply,
    },
    PlanReviewRespond {
        choice: agent::PlanReviewChoice,
        reply: Reply,
    },
    Cancel {
        reply: Reply,
    },
    ReadConversation {
        reply: Reply,
    },
    ReadMessages {
        reply: Reply,
    },
    IsRunning {
        reply: Reply,
    },
    AwaitIdle {
        timeout: Duration,
        reply: Reply,
    },
    Quit {
        reply: Reply,
    },
}

/// Spawn the gpui-side dispatcher. Consumes the `Receiver` paired with the
/// `Sender` handed to the MCP server. Holds the single target Workspace for
/// v1. When the channel closes (stdio EOF — the agent disconnected), the
/// dispatcher quits the app.
pub fn spawn_dispatcher(
    cx: &mut App,
    rx: async_channel::Receiver<McpRequest>,
    workspace: Entity<Workspace>,
) {
    cx.spawn(async move |cx: &mut AsyncApp| {
        while let Ok(req) = rx.recv().await {
            handle_request(req, &workspace, cx).await;
        }
        // Channel closed: the tokio-side server returned (stdio EOF). Quit.
        cx.update(|cx| cx.quit());
    })
    .detach();
}

async fn handle_request(req: McpRequest, workspace: &Entity<Workspace>, cx: &mut AsyncApp) {
    match req {
        McpRequest::NewThread { reply } => {
            cx.update(|cx| {
                if let Some(handle) = crate::dispatch::window_global() {
                    let _ = handle.update(cx, |_, window, cx| {
                        workspace.update(cx, |ws, cx| ws.harness_new_thread(window, cx));
                    });
                }
            });
            let _ = reply.send(Ok(json!({})));
        }
        McpRequest::OpenThread { id, reply } => {
            let opened = cx.update(|cx| {
                let Some(handle) = crate::dispatch::window_global() else {
                    return false;
                };
                handle
                    .update(cx, |_, window, cx| {
                        workspace.update(cx, |ws, cx| ws.harness_open_thread(id, window, cx))
                    })
                    .unwrap_or(false)
            });
            let _ = reply.send(Ok(json!({ "opened": opened })));
        }
        McpRequest::ListThreads { reply } => {
            let v = cx.update(|cx| Harness::new(workspace.clone()).list_threads(cx));
            let _ = reply.send(Ok(json!({ "threads": v })));
        }
        McpRequest::SendMessage { text, reply } => {
            let r: Result<(), String> =
                cx.update(|cx| workspace.update(cx, |ws, cx| ws.harness_send_message(text, cx)));
            let _ = reply.send(r.map(|_| json!({})));
        }
        McpRequest::SendCommand { name, args, reply } => {
            cx.update(|cx| workspace.update(cx, |ws, cx| ws.run_command_turn(&name, &args, cx)));
            let _ = reply.send(Ok(json!({})));
        }
        McpRequest::Approve { decision, reply } => {
            let had =
                cx.update(|cx| workspace.update(cx, |ws, cx| ws.harness_approve(decision, cx)));
            let _ = reply.send(Ok(json!({ "had_pending": had })));
        }
        McpRequest::PlanReviewRespond { choice, reply } => {
            let had = cx.update(|cx| {
                workspace.update(cx, |ws, cx| ws.harness_plan_review_respond(choice, cx))
            });
            let _ = reply.send(Ok(json!({ "had_pending": had })));
        }
        McpRequest::Cancel { reply } => {
            cx.update(|cx| workspace.update(cx, |ws, cx| ws.cancel_turn(cx)));
            let _ = reply.send(Ok(json!({})));
        }
        McpRequest::ReadConversation { reply } => {
            let v = cx.update(|cx| Harness::new(workspace.clone()).read_conversation(cx));
            let _ = reply.send(Ok(v));
        }
        McpRequest::ReadMessages { reply } => {
            let v = cx.update(|cx| Harness::new(workspace.clone()).read_messages(cx));
            let _ = reply.send(Ok(json!({ "messages": v })));
        }
        McpRequest::IsRunning { reply } => {
            let r = cx.update(|cx| Harness::new(workspace.clone()).is_running(cx));
            let _ = reply.send(Ok(json!({ "running": r })));
        }
        McpRequest::AwaitIdle { timeout, reply } => {
            let state = await_idle(workspace, cx, timeout).await;
            let _ = reply.send(Ok(json!({ "state": state })));
        }
        McpRequest::Quit { reply } => {
            let _ = reply.send(Ok(json!({})));
            cx.update(|cx| cx.quit());
        }
    }
}

/// Poll `Thread::is_running()` until idle or the deadline. The gpui-native
/// `BackgroundExecutor::timer` yields this future back to the gpui executor
/// without blocking it — and, unlike `tokio::time::sleep`, does not require a
/// tokio reactor to be active (the gpui background executor is not a tokio
/// context; spawning a tokio sleep there would panic with "no reactor
/// running"). Mirrors the live-test pattern at `thread.rs`.
async fn await_idle(
    workspace: &Entity<Workspace>,
    cx: &mut AsyncApp,
    timeout: Duration,
) -> IdleState {
    use std::time::Instant;
    let deadline = Instant::now() + timeout;
    loop {
        let running = cx.update(|cx| workspace.read(cx).thread.read(cx).is_running());
        if !running {
            return IdleState::Idle;
        }
        if Instant::now() > deadline {
            return IdleState::StillRunning;
        }
        cx.background_executor()
            .timer(Duration::from_millis(25))
            .await;
    }
}
