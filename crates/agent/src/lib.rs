//! Core agent logic for manox.
//!
//! `Thread` state machine + `LanguageModel` trait + tools + SQLite persistence,
//! gpui-native. The LLM connects directly to providers declared in
//! `~/.config/cx/cx.providers.config.yaml`.

pub mod agent_def;
pub mod db;
pub mod hashline;
pub mod language_model;
pub mod message;
pub mod paths;
pub mod provider;
pub mod runtime;
pub mod system_prompt;
pub mod thread;
pub mod thread_store;
pub mod tool;
pub mod tools;

use gpui::App;

pub use db::ThreadSummary;
pub use message::Message;
pub use thread::{Thread, ThreadEvent, ThreadId, ToolCallStatus};
pub use thread_store::{ThreadStore, ThreadStoreEvent, global as thread_store_global, save_thread};
pub use tool::permission::{PermissionCache, PermissionDecision, ToolAuthorizationResponse};
pub use tool::{AgentTool, AnyAgentTool, ToolOutputSink, ToolRegistry};

/// Register the tokio runtime + `ProviderRegistry` + `ThreadStore` + the
/// hashline snapshot store + the subagent definition registry. Call at App startup.
pub fn init(cx: &mut App) {
    runtime::init(cx);
    provider::registry::init(cx);
    thread_store::init(cx);
    hashline::init();
    agent_def::init();
}
