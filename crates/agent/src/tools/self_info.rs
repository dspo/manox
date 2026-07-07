//! The `self_info` tool: let the model read its own runtime identity on demand.
//!
//! Thread id, cwd, project, model, turn count, and depth are facts the model
//! occasionally needs (e.g. the user asks "what's the current thread id?").
//! Injecting them into the system prompt every request is wasteful and — for
//! thread id — unusual: codex routes it through MCP request meta, zed does not
//! expose it at all. manox persists threads in SQLite and surfaces them in the
//! sidebar, so users do ask; the tool lets the model answer without digging
//! into `threads.db` (the failure mode behind thread `c5aefe4d`, where the
//! agent ran `SELECT * FROM threads` and hallucinated off the truncated dump).
//!
//! Read-only, no approval. Reads the owning `Thread`'s identity through a
//! [`ToolContext`](crate::tool::ToolContext) snapshot built per call by
//! `run_tool_inner`, so the tool itself holds no `Thread` reference.

use std::sync::Arc;

use gpui::{App, AppContext as _, Task};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::tool::{AgentTool as AgentToolTrait, AnyAgentTool, ToolContext};

/// The `self_info` tool. Stateless — the runtime identity is read off the
/// `ToolContext` snapshot passed into `run`, not held as a `Thread` reference.
pub struct SelfInfoTool;

impl Default for SelfInfoTool {
    fn default() -> Self {
        Self
    }
}

impl SelfInfoTool {
    pub fn new() -> Self {
        Self
    }
}

#[derive(Deserialize, JsonSchema)]
struct SelfInfoInput {}

impl AgentToolTrait for SelfInfoTool {
    fn name(&self) -> &str {
        "self_info"
    }

    fn description(&self) -> &str {
        "Read the current agent's runtime identity: thread id, current working directory, \
         project root, model, turns used / cap, and nesting depth. Call this when the user \
         asks for the thread id or you need to confirm the runtime environment — do not run \
         SQL or dig into the persistence layer (threads.db) to look yourself up."
    }

    fn input_schema(&self) -> serde_json::Value {
        super::schema::<SelfInfoInput>()
    }

    fn requires_approval(&self, _input: &serde_json::Value) -> bool {
        false
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn run(
        &self,
        _input: serde_json::Value,
        _cancel: CancellationToken,
        ctx: &dyn ToolContext,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let model = ctx
            .model()
            .map(|m| format!("{} ({})", m.name(), m.provider_name()))
            .unwrap_or_else(|| "(none)".to_string());
        let project = ctx
            .project()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(unset)".to_string());
        let max = ctx
            .max_turns()
            .map(|m| m.to_string())
            .unwrap_or_else(|| "unlimited".to_string());
        let s = format!(
            "thread id: {}\ncwd: {}\nproject: {}\nmodel: {}\nturn: {}/{}\ndepth: {}",
            ctx.thread_id(),
            ctx.cwd().display(),
            project,
            model,
            ctx.turn_count(),
            max,
            ctx.depth(),
        );
        cx.background_spawn(async move { Ok(s) })
    }
}

/// Upcast helper for registry construction.
pub fn new() -> AnyAgentTool {
    Arc::new(SelfInfoTool) as AnyAgentTool
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_is_self_info() {
        // provider API tool name charset is [a-zA-Z0-9_-]; "self_info" is valid
        // and reads like `self.info()` in OOP style.
        let tool = SelfInfoTool::new();
        assert_eq!(tool.name(), "self_info");
    }

    #[test]
    fn description_mentions_no_sql() {
        // The c5aefe4d failure mode: agent ran SQL to find its own thread id.
        // The description must steer the model away from the persistence layer.
        let tool = SelfInfoTool::new();
        assert!(tool.description().contains("do not run SQL"));
    }

    /// `self_info` reads the runtime identity off a `ToolContext` snapshot
    /// built by `run_tool_inner`, not off a `WeakEntity<Thread>` held by the
    /// tool. This test drives `run` directly with a snapshot built from a
    /// restored `Thread` and asserts the identity is returned. The tool no
    /// longer touches the `Thread` entity from `run`, so the double-lease
    /// hazard that motivated the `cx.update` (not `this.update`) invocation
    /// path in `run_tool_inner` is gone — `run_tool_inner_self_info_does_not_double_lease`
    /// in `thread.rs` still guards the invocation path end-to-end.
    #[test]
    fn run_returns_thread_identity_from_snapshot() {
        use std::sync::{Arc, Mutex};

        crate::agent_def::init();

        let cx = gpui::TestAppContext::single();
        let thread = cx.update(|cx| {
            crate::thread::Thread::restore(
                crate::db::ThreadRecord::for_test("reg-4543a630", "/tmp", Vec::new()),
                None,
                cx,
            )
        });
        let snapshot =
            thread.read_with(&cx, |t, _| crate::tool::ToolContextSnapshot::from_thread(t));
        let tool = SelfInfoTool::new();

        let result: Arc<Mutex<Option<Result<String, String>>>> = Arc::new(Mutex::new(None));
        let r = result.clone();
        cx.spawn(|cx| {
            let task = cx.update(|cx| {
                tool.run(
                    serde_json::json!({}),
                    CancellationToken::new(),
                    &snapshot,
                    cx,
                )
            });
            async move {
                *r.lock().unwrap() = Some(task.await);
            }
        })
        .detach();
        cx.run_until_parked();

        let out = result
            .lock()
            .unwrap()
            .take()
            .expect("self_info task did not complete")
            .expect("self_info returned an error");
        assert!(
            out.contains("reg-4543a630"),
            "expected thread id in output, got: {out}"
        );
    }
}
