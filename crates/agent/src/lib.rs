//! Core agent logic for manox.
//!
//! `Thread` state machine + `LanguageModel` trait + tools + SQLite persistence,
//! gpui-native. The LLM connects directly to providers declared in
//! `~/.config/cx/cx.providers.config.yaml`.

pub mod agent_def;
pub mod command;
pub mod db;
pub mod frontmatter;
pub mod hashline;
pub mod hook;
pub mod language_model;
pub mod mcp;
pub mod message;
pub mod model_alias;
pub mod paths;
pub mod plugin;
pub mod provider;
pub mod runtime;
pub mod sandbox;
pub mod skill;
pub mod system_prompt;
pub mod thread;
pub mod thread_store;
pub mod title;
pub mod tool;
pub mod tools;

use gpui::App;

pub use db::ThreadSummary;
pub use mcp::{McpRegistry, registry_global as mcp_global, registry_init as mcp_init};
pub use message::Message;
pub use thread::{Thread, ThreadEvent, ThreadId, ToolCallStatus};
pub use thread_store::{ThreadStore, ThreadStoreEvent, global as thread_store_global, save_thread};
pub use tool::permission::{PermissionCache, PermissionDecision, ToolAuthorizationResponse};
pub use tool::{
    AgentTool, AnyAgentTool, PlanApprovalResponse, ToolOutputSink, ToolRegistry,
    exit_plan_mode_request_tool,
};

/// Register the tokio runtime, `ProviderRegistry`, `McpRegistry`,
/// `ThreadStore`, the hashline snapshot store, and the subagent / skill /
/// command / hook registries. Call at App startup.
pub fn init(cx: &mut App) {
    runtime::init(cx);
    provider::registry::init(cx);
    mcp::registry::init(cx);
    thread_store::init(cx);
    hashline::init();
    agent_def::init();
    skill::init();
    command::init();
    hook::init();
}
