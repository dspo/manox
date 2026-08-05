//! Core agent logic for manox.
//!
//! `Thread` state machine + `LanguageModel` trait + tools + SQLite persistence,
//! gpui-native. The LLM connects directly to providers declared in
//! `~/.config/cx/cx.providers.config.yaml`.

#[cfg(feature = "harness-manox")]
pub mod agent_def;
pub mod approval;
pub mod background_task;
#[cfg(feature = "harness-manox")]
pub mod claude_md;
#[cfg(feature = "harness-manox")]
pub mod collaboration_mode;
#[cfg(feature = "harness-manox")]
pub mod command;
pub mod compact;
pub mod db;
#[cfg(feature = "harness-manox")]
pub mod frontmatter;
pub mod goal;
#[cfg(feature = "harness-manox")]
pub mod hashline;
#[cfg(feature = "harness-manox")]
pub mod hook;
pub mod i18n;
#[cfg(feature = "harness-manox")]
pub mod image;
pub mod language;
pub mod language_model;
#[cfg(feature = "harness-manox")]
pub mod lsp;
#[cfg(feature = "harness-manox")]
pub mod mcp;
pub mod message;
pub mod model_alias;
#[cfg(feature = "harness-manox")]
pub mod optimizer;
#[cfg(feature = "harness-manox")]
pub mod path_env;
pub mod paths;
pub mod pi_bridge;
pub mod plan;
pub mod plugin;
#[cfg(feature = "harness-manox")]
pub mod prefix_stability;
pub mod prompt;
#[cfg(feature = "harness-manox")]
pub mod proposed_plan;
pub mod provider;
#[cfg(feature = "harness-manox")]
pub mod read_policy;
#[cfg(feature = "harness-manox")]
pub mod replay;
#[cfg(feature = "harness-manox")]
pub mod retention;
pub mod runtime;
#[cfg(feature = "harness-manox")]
pub mod sandbox;
pub mod settings;
#[cfg(feature = "harness-manox")]
pub mod skill;
#[cfg(feature = "harness-manox")]
pub mod system_prompt;
#[cfg(feature = "harness-manox")]
pub mod team;
#[cfg(feature = "harness-manox")]
pub mod title;
#[cfg(feature = "harness-manox")]
pub mod title_state;
#[cfg(feature = "harness-manox")]
pub mod token_meter;
#[cfg(feature = "harness-manox")]
pub mod tool;
pub mod tools;
#[cfg(feature = "harness-manox")]
pub mod turn_ext;
pub mod version;
pub mod webview_host;

pub mod thread;
pub mod thread_engine;
pub mod thread_store;

#[cfg(feature = "harness-pi")]
pub mod pi_engine;
use gpui::App;

#[cfg(feature = "harness-manox")]
pub use collaboration_mode::{PlanReviewChoice, implement_plan_user_message, unified_instructions};
pub use db::ThreadSummary;
pub use language_model::{ReasoningEffort, TokenUsage};
#[cfg(feature = "harness-manox")]
pub use mcp::{McpRegistry, registry_global as mcp_global, registry_init as mcp_init};
pub use message::{Message, MessageProvenance, MessageUiMetadata};
pub use plan::{PlanSnapshot, PlanStep, PlanStepStatus};
pub use thread::{SideCallMetric, Thread, ThreadEvent, ThreadId, ToolCallStatus};
#[cfg(feature = "harness-manox")]
pub use thread::PendingAuthMeta;
pub use thread_store::{ThreadStore, ThreadStoreEvent, global as thread_store_global, save_thread};
#[cfg(feature = "harness-manox")]
pub use tool::permission::{PermissionCache, PermissionDecision, ToolAuthorizationResponse};
#[cfg(feature = "harness-manox")]
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
    #[cfg(feature = "harness-manox")]
    mcp::registry::init(cx);
    // LSP PATH detection (no spawn — servers start lazily on first code-intel
    // call). Runs after MCP so the registry is settled before the first
    // `main_registry` build picks up LSP tools.
    #[cfg(feature = "harness-manox")]
    lsp::init();
    // Always initialize the global ThreadStore. In test-support builds the
    // real db is also opened, but `global()` checks `TEST_OVERRIDE` first —
    // tests that call `init_for_test` after `init` still get the in-memory
    // store over the real one. Skipping this call (the prior `#[cfg(not(…))]`
    // guard) caused a launch-time panic in `Sidebar::new` → `thread_store_global`
    // whenever the binary was built with `test-support` enabled.
    thread_store::init(cx);
    #[cfg(feature = "harness-manox")]
    hashline::init();
    #[cfg(feature = "harness-manox")]
    agent_def::init();
    #[cfg(feature = "harness-manox")]
    skill::init();
    #[cfg(feature = "harness-manox")]
    command::init();
    #[cfg(feature = "harness-manox")]
    hook::init();
}
