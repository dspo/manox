//! Core agent logic for manox.
//!
//! `Thread` state machine + `LanguageModel` trait + tools + SQLite persistence,
//! gpui-native. The LLM connects directly to providers declared in
//! `~/.config/cx/cx.providers.config.yaml`.

pub mod agent_def;
pub mod approval;
pub mod command;
pub mod compact;
pub mod db;
pub mod frontmatter;
pub mod goal;
pub mod hashline;
pub mod hook;
pub mod i18n;
pub mod image;
pub mod language_model;
pub mod mcp;
pub mod message;
pub mod model_alias;
pub mod path_env;
pub mod paths;
pub mod plugin;
pub mod prefix_stability;
pub mod provider;
pub mod runtime;
pub mod sandbox;
pub mod settings;
pub mod skill;
pub mod system_prompt;
pub mod team;
pub mod thread;
pub mod thread_store;
pub mod title;
pub mod title_state;
pub mod token_meter;
pub mod tool;
pub mod tools;

use gpui::App;

pub use db::ThreadSummary;
pub use language_model::{ReasoningEffort, TokenUsage};
pub use mcp::{McpRegistry, registry_global as mcp_global, registry_init as mcp_init};
pub use message::{Message, MessageUiMetadata};
pub use thread::{PendingAuthMeta, Thread, ThreadEvent, ThreadId, ToolCallStatus};
pub use thread_store::{ThreadStore, ThreadStoreEvent, global as thread_store_global, save_thread};
pub use tool::permission::{PermissionCache, PermissionDecision, ToolAuthorizationResponse};
pub use tool::{
    AgentTool, AnyAgentTool, PlanApprovalResponse, ToolOutputSink, ToolRegistry,
    enter_plan_mode_request_tool, exit_plan_mode_request_tool,
};

/// Register the tokio runtime, `ProviderRegistry`, `McpRegistry`,
/// `ThreadStore`, the hashline snapshot store, the i18n bundle, and the
/// subagent / skill / command / hook registries. Call at App startup.
pub fn init(cx: &mut App) {
    runtime::init(cx);
    // i18n before anything that renders UI or builds a system prompt, so the
    // user's locale is settled before the first frame / first turn.
    i18n::init();
    provider::registry::init(cx);
    mcp::registry::init(cx);
    thread_store::init(cx);
    hashline::init();
    agent_def::init();
    skill::init();
    command::init();
    hook::init();
}
