//! Debug Harness: a programmatic driver for a manox `Workspace`.
//!
//! The Harness wraps an `Entity<Workspace>` and exposes high-level operations
//! (send a message, approve a tool, read the conversation) as plain Rust
//! methods that need no `&mut Window` and no physical input. It is the shared
//! core consumed by two front-ends:
//!
//! 1. `cargo test` — tests call the sync methods directly against a
//!    `TestAppContext` workspace.
//! 2. the MCP server (`manox --mcp`) — a tokio↔gpui bridge (see `bridge.rs`)
//!    dispatches MCP tool calls to these same methods via `cx.update`.
//!
//! The Harness lives inside the `agent-ui` crate (rather than a separate
//! crate) because it must reach `pub(crate)` Workspace methods.

pub mod bridge;
pub mod types;

#[cfg(all(test, feature = "debug"))]
pub mod test_support;
#[cfg(all(test, feature = "debug"))]
mod tests;

pub use types::{IdleState, MessageSnapshot, ThreadInfo};

#[cfg(all(test, feature = "debug"))]
use std::time::Duration;

use agent::language_model::Role;
use agent::{PermissionDecision, thread_store_global};
use gpui::{App, Entity};
use serde_json::json;

use crate::conversation::ConvItem;
use crate::workspace::Workspace;

/// Programmatic handle to a manox `Workspace`. Clone-cheap (holds one entity
/// handle). All methods take `&self` and mutate through `Entity::update`.
#[derive(Clone)]
pub struct Harness {
    workspace: Entity<Workspace>,
}

impl Harness {
    /// Wrap an existing Workspace entity. The caller is responsible for
    /// constructing the Workspace (real window, `TestAppContext`, or the MCP
    /// dispatcher's per-session map).
    pub fn new(workspace: Entity<Workspace>) -> Self {
        Self { workspace }
    }

    pub fn workspace(&self) -> Entity<Workspace> {
        self.workspace.clone()
    }

    pub fn thread_id(&self, cx: &App) -> String {
        self.workspace.read(cx).thread.read(cx).id.0.clone()
    }

    pub fn is_running(&self, cx: &App) -> bool {
        self.workspace.read(cx).thread.read(cx).is_running()
    }

    /// Set the composer-equivalent user text and submit, starting a model
    /// turn. Refused (returns `Err`) if a turn is already running.
    pub fn send_message(&self, text: String, cx: &mut App) -> Result<(), String> {
        self.workspace
            .update(cx, |ws, cx| ws.harness_send_message(text, cx))
    }

    /// Run a markdown prompt-macro slash command turn (`/name args`).
    pub fn send_command(&self, name: &str, args: &str, cx: &mut App) -> Result<(), String> {
        self.workspace
            .update(cx, |ws, cx| ws.run_command_turn(name, args, cx));
        Ok(())
    }

    /// Resolve the pending tool-call authorization. Returns whether one was
    /// pending (and thus consumed).
    pub fn approve(&self, decision: PermissionDecision, cx: &mut App) -> Result<bool, String> {
        Ok(self
            .workspace
            .update(cx, |ws, cx| ws.harness_approve(decision, cx)))
    }

    /// Approve or reject the pending `exit_plan_mode` plan. Returns whether
    /// a plan was pending.
    pub fn plan_respond(&self, approve: bool, cx: &mut App) -> Result<bool, String> {
        Ok(self
            .workspace
            .update(cx, |ws, cx| ws.harness_plan_respond(approve, cx)))
    }

    pub fn cancel(&self, cx: &mut App) -> Result<(), String> {
        self.workspace.update(cx, |ws, cx| ws.cancel_turn(cx));
        Ok(())
    }

    pub fn new_thread(&self, cx: &mut App) -> Result<(), String> {
        self.workspace
            .update(cx, |ws, cx| ws.harness_new_thread(cx));
        Ok(())
    }

    pub fn open_thread(&self, id: String, cx: &mut App) -> Result<bool, String> {
        Ok(self
            .workspace
            .update(cx, |ws, cx| ws.harness_open_thread(id, cx)))
    }

    /// All persisted threads, in sidebar order.
    pub fn list_threads(&self, cx: &App) -> Vec<ThreadInfo> {
        thread_store_global()
            .read(cx)
            .summaries()
            .iter()
            .map(|s| ThreadInfo {
                id: s.id.clone(),
                title: s
                    .title_override
                    .clone()
                    .or_else(|| s.title.clone())
                    .unwrap_or_else(|| s.summary.clone()),
            })
            .collect()
    }

    /// The rendered conversation: each `ConvItem` serialized to a JSON object
    /// with its `kind` and fields. This is the agent-facing view of what the
    /// user sees.
    pub fn read_conversation(&self, cx: &App) -> serde_json::Value {
        let items = self.workspace.read(cx).conversation.read(cx).items();
        let out: Vec<serde_json::Value> = items
            .iter()
            .map(|e| {
                let item = e.read(cx);
                match item.kind() {
                    ConvItem::User(t) => json!({ "kind": "user", "text": t }),
                    ConvItem::Assistant {
                        text, streaming, ..
                    } => {
                        json!({ "kind": "assistant", "text": text, "streaming": streaming })
                    }
                    ConvItem::Reasoning {
                        text,
                        streaming,
                        collapsed,
                        ..
                    } => json!({
                        "kind": "reasoning",
                        "text": text,
                        "streaming": streaming,
                        "collapsed": collapsed,
                    }),
                    ConvItem::ToolCall(tc) => json!({
                        "kind": "tool_call",
                        "id": tc.id,
                        "name": tc.name,
                        "title": tc.title,
                        "status": format!("{:?}", tc.status),
                        "output": tc.output,
                        "is_error": tc.is_error,
                        "streaming": tc.streaming,
                    }),
                    ConvItem::AgentTask(a) => json!({
                        "kind": "agent_task",
                        "id": a.id,
                        "title": a.title,
                        "streaming": a.streaming,
                        "is_error": a.is_error,
                        "final_text": a.final_text,
                    }),
                    ConvItem::Error(t) => json!({ "kind": "error", "text": t }),
                    ConvItem::Notice(t) => json!({ "kind": "notice", "text": t }),
                }
            })
            .collect();
        json!({ "items": out })
    }

    /// The canonical `Thread::messages()`, reduced to role + flattened text.
    /// This is the source of truth persisted to the db, as opposed to the
    /// rendered `read_conversation` view.
    pub fn read_messages(&self, cx: &App) -> Vec<MessageSnapshot> {
        self.workspace
            .read(cx)
            .thread
            .read(cx)
            .messages()
            .iter()
            .map(|m| {
                let role = match m.role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                    Role::System => "system",
                };
                let text = m
                    .content
                    .iter()
                    .filter_map(|c| c.to_str().map(String::from))
                    .collect::<Vec<_>>()
                    .join("\n");
                MessageSnapshot {
                    role: role.into(),
                    text,
                }
            })
            .collect()
    }
}

/// Poll-driven idle wait usable from a sync caller that can pump the gpui
/// executor (a `TestAppContext` test). Mirrors the live-test pattern at
/// `thread.rs:~2927`: poll `is_running()`, run until parked, bound by a
/// deadline. Real-window MCP callers use `bridge::await_idle` instead.
#[cfg(all(test, feature = "debug"))]
pub fn await_idle_sync(
    workspace: &Entity<Workspace>,
    cx: &mut gpui::TestAppContext,
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
        cx.run_until_parked();
        // TestAppContext::run_until_parked advances pending tasks; if the turn
        // is still running (e.g. a live HTTP stream), park returns without
        // completing it. Yield the remainder of the slice via a short park
        // retry loop bounded by the deadline above.
        std::thread::sleep(Duration::from_millis(10));
    }
}
