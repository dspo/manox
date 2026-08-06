//! Core agent logic for manox.
//!
//! `Thread` state machine + `LanguageModel` trait + tools + SQLite persistence,
//! gpui-native. The LLM connects directly to providers declared in
//! `~/.config/cx/cx.providers.config.yaml`.

#[cfg(not(feature = "harness-pi"))]
pub mod agent_def;
pub mod approval;
pub mod background_task;
#[cfg(not(feature = "harness-pi"))]
pub mod claude_md;
#[cfg(not(feature = "harness-pi"))]
pub mod collaboration_mode;
#[cfg(not(feature = "harness-pi"))]
pub mod command;
pub mod compact;
pub mod db;
#[cfg(not(feature = "harness-pi"))]
pub mod frontmatter;
pub mod goal;
#[cfg(not(feature = "harness-pi"))]
pub mod hashline;
#[cfg(not(feature = "harness-pi"))]
pub mod hook;
pub mod i18n;
#[cfg(not(feature = "harness-pi"))]
pub mod image;
pub mod language;
pub mod language_model;
#[cfg(not(feature = "harness-pi"))]
pub mod lsp;
#[cfg(not(feature = "harness-pi"))]
pub mod mcp;
pub mod message;
pub mod model_alias;
#[cfg(not(feature = "harness-pi"))]
pub mod optimizer;
#[cfg(not(feature = "harness-pi"))]
pub mod path_env;
pub mod paths;
pub mod pi_bridge;
pub mod plan;
pub mod plugin;
#[cfg(not(feature = "harness-pi"))]
pub mod prefix_stability;
pub mod prompt;
#[cfg(not(feature = "harness-pi"))]
pub mod proposed_plan;
pub mod provider;
#[cfg(not(feature = "harness-pi"))]
pub mod read_policy;
#[cfg(not(feature = "harness-pi"))]
pub mod replay;
#[cfg(not(feature = "harness-pi"))]
pub mod retention;
pub mod runtime;
#[cfg(not(feature = "harness-pi"))]
pub mod sandbox;
pub mod settings;
#[cfg(not(feature = "harness-pi"))]
pub mod skill;
#[cfg(not(feature = "harness-pi"))]
pub mod system_prompt;
#[cfg(not(feature = "harness-pi"))]
pub mod team;
#[cfg(not(feature = "harness-pi"))]
pub mod title;
#[cfg(not(feature = "harness-pi"))]
pub mod title_state;
#[cfg(not(feature = "harness-pi"))]
pub mod token_meter;
#[cfg(not(feature = "harness-pi"))]
pub mod tool;
pub mod tools;
#[cfg(not(feature = "harness-pi"))]
pub mod turn_ext;
pub mod version;
pub mod webview_host;

pub mod thread;
pub mod thread_engine;
pub mod thread_store;

#[cfg(feature = "harness-pi")]
pub mod pi_engine;
use gpui::App;

#[cfg(not(feature = "harness-pi"))]
pub use collaboration_mode::{PlanReviewChoice, implement_plan_user_message, unified_instructions};
pub use db::ThreadSummary;
pub use language_model::{ReasoningEffort, TokenUsage};
#[cfg(not(feature = "harness-pi"))]
pub use mcp::{McpRegistry, registry_global as mcp_global, registry_init as mcp_init};
pub use message::{Message, MessageProvenance, MessageUiMetadata};
pub use plan::{PlanSnapshot, PlanStep, PlanStepStatus};
#[cfg(not(feature = "harness-pi"))]
pub use thread::PendingAuthMeta;
pub use thread::{SideCallMetric, Thread, ThreadEvent, ThreadId, ToolCallStatus};
pub use thread_store::{ThreadStore, ThreadStoreEvent, global as thread_store_global, save_thread};
#[cfg(not(feature = "harness-pi"))]
pub use tool::permission::{PermissionCache, PermissionDecision, ToolAuthorizationResponse};
#[cfg(not(feature = "harness-pi"))]
pub use tool::{AgentTool, AnyAgentTool, ToolOutputSink, ToolRegistry};

/// Register the tokio runtime, `ProviderRegistry`, `McpRegistry`,
/// `ThreadStore`, the hashline snapshot store, the i18n bundle, and the
/// subagent / skill / command / hook registries. Call at App startup.
pub fn init(cx: &mut App) {
    runtime::init(cx);
    // i18n before anything that renders UI or builds a system prompt, so the
    // user's locale is settled before the first frame / first turn.
    i18n::init();
    settings::init_optimization();
    provider::registry::init(cx);
    #[cfg(not(feature = "harness-pi"))]
    mcp::registry::init(cx);
    // LSP PATH detection (no spawn — servers start lazily on first code-intel
    // call). Runs after MCP so the registry is settled before the first
    // `main_registry` build picks up LSP tools.
    #[cfg(not(feature = "harness-pi"))]
    lsp::init();
    // Always initialize the global ThreadStore. In test-support builds the
    // real db is also opened, but `global()` checks `TEST_OVERRIDE` first —
    // tests that call `init_for_test` after `init` still get the in-memory
    // store over the real one. Skipping this call (the prior `#[cfg(not(…))]`
    // guard) caused a launch-time panic in `Sidebar::new` → `thread_store_global`
    // whenever the binary was built with `test-support` enabled.
    thread_store::init(cx);
    #[cfg(not(feature = "harness-pi"))]
    hashline::init();
    #[cfg(not(feature = "harness-pi"))]
    agent_def::init();
    #[cfg(not(feature = "harness-pi"))]
    skill::init();
    #[cfg(not(feature = "harness-pi"))]
    command::init();
    #[cfg(not(feature = "harness-pi"))]
    hook::init();
}
