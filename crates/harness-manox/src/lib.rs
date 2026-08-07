//! The manox self-built harness — retired, preserved as a standalone crate.
//!
//! This crate archives the pre-pi architecture (the manox harness loop, its
//! provider streaming stack, MCP/LSP/skill/command wiring, and the manox
//! `Thread` implementation). The shipped `manox` binary runs the pi harness
//! exclusively; nothing here is reachable from it. The crate stays in the
//! workspace and keeps compiling so the code survives as a faithful archive
//! (see the Stage-5 retirement in the pi provider-registry migration).

pub mod agent_def;
pub mod approval_review;
pub mod claude_md;
pub mod collaboration_mode;
pub mod command;
pub mod compaction_calls;
pub mod frontmatter;
pub mod hashline;
pub mod hook;
pub mod image;
pub mod language_model;
pub mod lsp;
pub mod mcp;
pub mod model_alias;
pub mod optimizer;
pub mod path_env;
pub mod prefix_stability;
pub mod proposed_plan;
pub mod provider;
pub mod read_policy;
pub mod replay;
pub mod retention;
pub mod sandbox;
pub mod settings_ext;
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
pub mod turn_ext;

pub use agent::language_model::{ReasoningEffort, TokenUsage};
pub use collaboration_mode::{PlanReviewChoice, implement_plan_user_message, unified_instructions};
pub use mcp::{McpRegistry, registry_global as mcp_global, registry_init as mcp_init};
pub use thread::PendingAuthMeta;
pub use thread::{SideCallMetric, Thread, ThreadEvent, ThreadId, ToolCallStatus};
pub use thread_store::{ThreadStore, ThreadStoreEvent, global as thread_store_global, save_thread};
pub use tool::permission::{PermissionCache, PermissionDecision, ToolAuthorizationResponse};
pub use tool::{AgentTool, AnyAgentTool, ToolOutputSink, ToolRegistry};

/// The manox-harness startup sequence — the manox-specific half of the old
/// `agent::init`. Dead code since the pi harness became the only shipped
/// path; kept so the archive stays runnable in principle.
pub fn init(cx: &mut gpui::App) {
    provider::registry::init(cx);
    mcp::registry::init(cx);
    lsp::init();
    hashline::init();
    agent_def::init();
    skill::init();
    command::init();
    hook::init();
}
