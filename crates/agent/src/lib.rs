//! Core agent logic for manox.
//!
//! `Thread` state machine + `LanguageModel` trait + tools + SQLite persistence,
//! gpui-native. The LLM connects directly to providers declared in
//! `~/.manox/cx.providers.config.yaml`.

pub mod agent_defs;
pub mod approval;
pub mod approval_review;
pub mod background_task;
pub mod chrome_use;
pub mod claude_md;
pub mod collaboration_mode;
pub mod command;
pub mod db;
pub mod file_lock;
pub mod frontmatter;
pub mod goal;
pub mod goal_driver;
pub mod goal_tools;
pub mod host;
pub mod i18n;
pub mod image;
pub mod language;
pub mod language_model;
pub mod lsp_tools;
pub mod mcp;
pub mod message;
pub mod path_env;
pub mod path_policy;
pub mod paths;
pub mod permission;
pub mod pi_providers;
pub mod plan;
pub mod plan_mode;
pub mod plugin;
pub mod plugin_hooks;
pub mod prompt;
pub mod proposed_plan;
pub mod provider;
pub mod runtime;
pub mod sailor_manager;
pub mod sandbox;
pub mod settings;
pub mod skill;
pub mod slash_builtins;
pub mod team;
pub mod title;
pub mod tools;
pub mod version;
pub mod web_fetch;
pub mod web_tools;
pub mod webview_host;
pub mod worktree;

pub mod thread;
pub mod thread_engine;
pub mod thread_store;

pub mod monitor_bridge;
pub mod pi_approval;
pub mod pi_engine;
use gpui::App;

pub use db::ThreadSummary;
pub use language_model::{ReasoningEffort, TokenUsage};
pub use message::{Message, MessageAuthor, MessageProvenance, MessageUiMetadata};
pub use permission::{
    PendingAuthMeta, PermissionCache, PermissionDecision, ToolAuthorizationResponse,
};
pub use plan::{PlanSnapshot, PlanStep, PlanStepStatus};
pub use thread::{
    SideCallMetric, SubagentChildEvent, Thread, ThreadEvent, ThreadId, ToolCallStatus,
};
pub use thread_store::{
    ThreadStore, ThreadStoreEvent, global as thread_store_global, refresh_thread_list,
};

/// Register the tokio runtime, `ProviderRegistry`, `McpRegistry`,
/// `ThreadStore`, the hashline snapshot store, the i18n bundle, and the
/// subagent / skill / command / hook registries. Call at App startup.
pub fn init(cx: &mut App) {
    // Login-shell PATH install (background): GUI processes inherit a minimal
    // launchd PATH, so bash/LSP/MCP/monitor subprocesses would lose Homebrew
    // binaries. Resolved once and applied process-wide; first thing so later
    // init work (provider shell credentials, MCP spawns) benefits as soon as
    // the resolver lands.
    path_env::install();
    runtime::init(cx);
    // i18n before anything that renders UI or builds a system prompt, so the
    // user's locale is settled before the first frame / first turn.
    i18n::init();
    settings::init_optimization();
    pi_providers::init();
    // MCP servers (mcp.toml + plugin .mcp.json layers) — blocks until the
    // connections settle (per-server timeout); failures are isolated.
    mcp::init();
    // LSP registry PATH probe on a background thread (sessions await it
    // bounded before registering the read-only LSP tools).
    lsp_tools::init_background();
    // Skill/command definition registries (markdown files from plugins and
    // the user config dir) — consumed by the slash-command dispatch and the
    // composer mention surface.
    skill::init();
    command::init();
    // Plugin lifecycle hooks (hooks/hooks.json) — loaded once; fired from
    // the engine (SessionStart/Stop/PreToolUse/PostToolUse) and the thread
    // store (SessionEnd on archive).
    plugin_hooks::init();
    // LSP PATH detection (no spawn — servers start lazily on first code-intel
    // call). Runs after MCP so the registry is settled before the first
    // `main_registry` build picks up LSP tools.
    // Always initialize the global ThreadStore. In test-support builds the
    // real db is also opened, but `global()` checks `TEST_OVERRIDE` first —
    // tests that call `init_for_test` after `init` still get the in-memory
    // store over the real one. Skipping this call (the prior `#[cfg(not(…))]`
    // guard) caused a launch-time panic in `Sidebar::new` → `thread_store_global`
    // whenever the binary was built with `test-support` enabled.
    thread_store::init(cx);
}
