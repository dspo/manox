//! Cx Agent — in-process coding agent.
//!
//! 入口：`run_cx_agent(selection, args)`，由 `lib.rs` 在 launcher 选中
//! `agent_id == "cx-agent"` 时分发进来。
//!
//! 模块布局：
//! - `runtime`  — tokio current_thread runtime
//! - `events`   — `CxStreamEvent` IR
//! - `history`  — `CxMessage` / `CxContent`
//! - `approval` — `ApprovalMode` / `ToolCategory` / `ApprovalDecision`
//! - `config`   — Selection → ProviderAdapterConfig 映射
//! - `provider` — `ProviderAdapter` trait + rig-core 适配
//! - `rollout`  — jsonl 持久化
//! - `session`  — turn loop（Phase 1.2 起）
//! - `tools`    — Tool trait + 6 个内置工具（Phase 2 起）
//! - `tui`      — chat 界面（Phase 1.3 起）

pub(crate) mod approval;
pub(crate) mod config;
pub(crate) mod events;
pub(crate) mod history;
pub(crate) mod provider;
pub(crate) mod rollout;
pub(crate) mod runtime;
pub(crate) mod session;
pub(crate) mod tools;
pub(crate) mod tui;

use anyhow::Result;

use crate::Selection;

/// Cx Agent 入口。被 `run_launcher` 在 `selection.agent_id == "cx-agent"` 时调用。
///
/// `passthrough_args` 是 launcher 透传给 agent 的命令行参数；v0 不消费，但保留参数面以便 v1。
pub fn run_cx_agent(selection: Selection, passthrough_args: Vec<String>) -> Result<()> {
    let runtime = runtime::build()?;
    runtime.block_on(async move { session::run(selection, passthrough_args).await })
}
