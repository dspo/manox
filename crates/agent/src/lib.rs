//! Core agent logic for manox.
//!
//! `Thread` state machine + `LanguageModel` trait + tools + SQLite persistence,
//! gpui-native. The LLM connects directly to providers declared in
//! `~/.config/cx/cx.providers.config.yaml`.

pub mod db;
pub mod language_model;
pub mod message;
pub mod provider;
pub mod runtime;
pub mod thread;
pub mod thread_store;
pub mod tool;
pub mod tools;

use gpui::App;

pub use message::Message;
pub use db::ThreadSummary;
pub use thread::{Thread, ThreadEvent, ThreadId, ToolCallStatus};
pub use thread_store::{ThreadStore, ThreadStoreEvent, global as thread_store_global, save_thread};
pub use tool::{AgentTool, AnyAgentTool, ToolRegistry};
pub use tool::permission::{PermissionCache, PermissionDecision};

/// Register the tokio runtime + `ProviderRegistry` + `ThreadStore`. Call at App startup.
pub fn init(cx: &mut App) {
    runtime::init(cx);
    provider::registry::init(cx);
    thread_store::init(cx);
}
