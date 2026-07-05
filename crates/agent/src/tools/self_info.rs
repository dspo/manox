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
}
