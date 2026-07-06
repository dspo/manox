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
//! Read-only, no approval. Holds a `WeakEntity<Thread>` to the owning thread,
//! mirroring the `agent` tool's pattern.

use std::sync::Arc;

use gpui::{App, AppContext as _, Task, WeakEntity};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::thread::Thread;
use crate::tool::{AgentTool as AgentToolTrait, AnyAgentTool};

/// The `self_info` tool. `thread` weakly references the `Thread` that owns
/// this tool so it can read the live runtime identity.
pub struct SelfInfoTool {
    thread: WeakEntity<Thread>,
}

impl SelfInfoTool {
    pub fn new(thread: WeakEntity<Thread>) -> Self {
        Self { thread }
    }
}

#[derive(Deserialize, JsonSchema)]
struct SelfInfoInput {}

impl AgentToolTrait for SelfInfoTool {
    fn name(&self) -> &str {
        "self_info"
    }

    fn description(&self) -> &str {
        "查看当前 agent 的运行时身份：thread id、当前工作目录、项目根、模型、\
         已用轮次/上限、嵌套深度。当用户问起 thread id 或你需要确认运行环境时调用——\
         不要跑 SQL 或翻持久层（threads.db）反查自身。"
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
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let body = self.thread.read_with(cx, |t, _| {
            let model = t
                .model()
                .map(|m| format!("{} ({})", m.name(), m.provider_name()))
                .unwrap_or_else(|| "(none)".to_string());
            let project = t
                .project()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "(unset)".to_string());
            let max = t
                .max_turns()
                .map(|m| m.to_string())
                .unwrap_or_else(|| "unlimited".to_string());
            format!(
                "thread id: {}\ncwd: {}\nproject: {}\nmodel: {}\nturn: {}/{}\ndepth: {}",
                t.id.0,
                t.cwd().display(),
                project,
                model,
                t.turn_count(),
                max,
                t.depth(),
            )
        });
        let s = body.unwrap_or_else(|_| "thread unavailable".to_string());
        cx.background_spawn(async move { Ok(s) })
    }
}

/// Upcast helper for registry construction.
pub fn new(thread: WeakEntity<Thread>) -> AnyAgentTool {
    Arc::new(SelfInfoTool::new(thread)) as AnyAgentTool
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_is_self_info() {
        // provider API tool name charset is [a-zA-Z0-9_-]; "self_info" is valid
        // and reads like `self.info()` in OOP style.
        let tool = SelfInfoTool::new(WeakEntity::<Thread>::new_invalid());
        assert_eq!(tool.name(), "self_info");
    }

    #[test]
    fn description_mentions_no_sql() {
        // The c5aefe4d failure mode: agent ran SQL to find its own thread id.
        // The description must steer the model away from the persistence layer.
        let tool = SelfInfoTool::new(WeakEntity::<Thread>::new_invalid());
        assert!(tool.description().contains("不要跑 SQL"));
    }

    /// `self_info` reads the owning `Thread` via `read_with`. The owning
    /// `Thread`'s `run_tool_inner` invokes tools through `cx.update` (App
    /// context, no entity lease) rather than `this.update` (which would hold
    /// a write lease on that same Thread) precisely so this `read_with` does
    /// not re-lease it and trip gpui's `double_lease_panic` — the SIGABRT
    /// captured in thread `4543a630`. This test locks in the safe invocation
    /// path: construct via `Thread::restore`, call `run` from `cx.update`, and
    /// assert the identity is returned. `agent_def::init()` is required only
    /// because `default_registry` eagerly registers the `agent` spawn tool,
    /// which reads the subagent definition registry.
    #[test]
    fn run_returns_thread_identity_without_double_lease() {
        use std::sync::{Arc, Mutex};

        crate::agent_def::init();

        let cx = gpui::TestAppContext::single();
        let thread = cx.update(|cx| {
            crate::thread::Thread::restore(
                crate::thread::ThreadId("reg-4543a630".to_string()),
                std::path::PathBuf::from("/tmp"),
                None,
                false,
                Vec::new(),
                None,
                cx,
            )
        });
        let tool = SelfInfoTool::new(thread.downgrade());

        let result: Arc<Mutex<Option<Result<String, String>>>> = Arc::new(Mutex::new(None));
        let r = result.clone();
        cx.spawn(|cx| {
            let task =
                cx.update(|cx| tool.run(serde_json::json!({}), CancellationToken::new(), cx));
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

    /// Documents the gpui invariant the `run_tool_inner` fix relies on:
    /// holding a write lease on the owning `Thread` (via `Entity::update`)
    /// while `self_info`'s `run` does `read_with` on the same entity
    /// re-leases it and trips `double_lease_panic`. This is the SIGABRT from
    /// thread `4543a630`, reproduced synchronously here without going through
    /// `run_tool_inner` (which would need a full turn). It does not guard the
    /// fix directly — it pins the invariant so a future re-introduction of
    /// `this.update` wrapping in `run_tool_inner` has a concrete, failing
    /// reference for why it breaks.
    #[test]
    #[should_panic(expected = "already being updated")]
    #[allow(clippy::let_underscore_future)]
    fn read_with_inside_owning_thread_write_lease_panics() {
        crate::agent_def::init();

        let cx = gpui::TestAppContext::single();
        let thread = cx.update(|cx| {
            crate::thread::Thread::restore(
                crate::thread::ThreadId("double-lease-doc".to_string()),
                std::path::PathBuf::from("/tmp"),
                None,
                false,
                Vec::new(),
                None,
                cx,
            )
        });
        let tool = SelfInfoTool::new(thread.downgrade());
        let thread_handle = thread.clone();

        cx.update(|cx| {
            thread_handle.update(cx, |_t, cx| {
                let _ = tool.run(serde_json::json!({}), CancellationToken::new(), cx);
            });
        });
    }
}
