//! Core agent logic for manox.
//!
//! `Thread` state machine + `LanguageModel` trait + tools + SQLite persistence,
//! gpui-native. The LLM connects directly to providers declared in
//! `~/.config/cx/cx.providers.config.yaml`.

pub mod agent_def;
pub mod approval;
pub mod claude_md;
pub mod collaboration_mode;
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
pub mod lsp;
pub mod mcp;
pub mod message;
pub mod model_alias;
pub mod path_env;
pub mod paths;
pub mod plugin;
pub mod prefix_stability;
pub mod prompt;
pub mod proposed_plan;
pub mod provider;
pub mod read_policy;
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
pub mod webview_host;

use gpui::App;

pub use collaboration_mode::{
    ModeKind, ModeSettings, ModeSettingsMap, PlanReviewChoice, implement_plan_user_message,
};
pub use db::ThreadSummary;
pub use language_model::{ReasoningEffort, TokenUsage};
pub use mcp::{McpRegistry, registry_global as mcp_global, registry_init as mcp_init};
pub use message::{Message, MessageUiMetadata};
pub use thread::{PendingAuthMeta, Thread, ThreadEvent, ThreadId, ToolCallStatus};
pub use thread_store::{ThreadStore, ThreadStoreEvent, global as thread_store_global, save_thread};
pub use tool::permission::{PermissionCache, PermissionDecision, ToolAuthorizationResponse};
pub use tool::{AgentTool, AnyAgentTool, ToolOutputSink, ToolRegistry};

/// Register the tokio runtime, `ProviderRegistry`, `McpRegistry`,
/// `ThreadStore`, the hashline snapshot store, the i18n bundle, and the
/// subagent / skill / command / hook registries. Call at App startup.
pub fn init(cx: &mut App) {
    runtime::init(cx);
    // i18n before anything that renders UI or builds a system prompt, so the
    // user's locale is settled before the first frame / first turn.
    i18n::init();
    settings::init_modes();
    provider::registry::init(cx);
    mcp::registry::init(cx);
    // LSP PATH detection (no spawn — servers start lazily on first code-intel
    // call). Runs after MCP so the registry is settled before the first
    // `main_registry` build picks up LSP tools.
    lsp::init();
    // The store opens the real `threads.db` into an un-clearable `OnceLock`,
    // which leaks the entity in the first test to call init. Tests use
    // `thread_store::init_for_test` (a clearable `TEST_OVERRIDE`) instead, so
    // skip the production init in test-support builds.
    #[cfg(not(feature = "test-support"))]
    thread_store::init(cx);
    hashline::init();
    agent_def::init();
    skill::init();
    command::init();
    hook::init();
}
