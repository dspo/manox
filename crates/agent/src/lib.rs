//! Core agent logic for manox.
//!
//! `Thread` state machine + `LanguageModel` trait + tools + SQLite persistence,
//! gpui-native. The LLM connects directly to providers declared in
//! `~/.config/cx/cx.providers.config.yaml`.

pub mod approval;
pub mod approval_review;
pub mod background_task;
pub mod compact;
pub mod db;
pub mod goal;
pub mod i18n;
pub mod language;
pub mod language_model;
pub mod message;
pub mod paths;
pub mod permission;
pub mod pi_providers;
pub mod plan;
pub mod plugin;
pub mod prompt;
pub mod provider;
pub mod runtime;
pub mod settings;
pub mod title;
pub mod tools;
pub mod version;
pub mod webview_host;

pub mod thread;
pub mod thread_engine;
pub mod thread_store;

pub mod pi_approval;
pub mod pi_engine;
use gpui::App;

pub use db::ThreadSummary;
pub use language_model::{ReasoningEffort, TokenUsage};
pub use message::{Message, MessageProvenance, MessageUiMetadata};
pub use permission::{
    PendingAuthMeta, PermissionCache, PermissionDecision, ToolAuthorizationResponse,
};
pub use plan::{PlanSnapshot, PlanStep, PlanStepStatus};
pub use thread::{SideCallMetric, Thread, ThreadEvent, ThreadId, ToolCallStatus};
pub use thread_store::{ThreadStore, ThreadStoreEvent, global as thread_store_global, save_thread};

/// Register the tokio runtime, `ProviderRegistry`, `McpRegistry`,
/// `ThreadStore`, the hashline snapshot store, the i18n bundle, and the
/// subagent / skill / command / hook registries. Call at App startup.
pub fn init(cx: &mut App) {
    runtime::init(cx);
    // i18n before anything that renders UI or builds a system prompt, so the
    // user's locale is settled before the first frame / first turn.
    i18n::init();
    settings::init_optimization();
    pi_providers::init();
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
