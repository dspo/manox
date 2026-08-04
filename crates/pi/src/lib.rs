// Pi agent harness — Rust port of the Pi coding agent's core loop, compaction,
// session management, and built-in tools.
//
// Layers (bottom-up):
//   agent_loop.rs  — run_loop: the pure dual-loop state machine
//   agent.rs — Agent: stateful wrapper with event subscription and queues
//   harness  — AgentHarness: orchestration layer with session persistence and hooks
//
// The two external abstractions are:
//   StreamFn      — how the harness calls an LLM (provider-agnostic)
//   ExecutionEnv  — how the harness accesses filesystem and shell

pub mod cache_stats;
pub mod coding_agent;
pub mod compaction;
pub mod env;
pub mod harness;
pub mod hashline;
pub mod output_guard;
pub mod provider;
pub mod session;
pub mod settings;
pub mod system_prompt;
pub mod tool;
pub mod tools;
pub mod trust;
pub mod types;

pub mod agent;
pub mod agent_loop;

pub use agent::Agent;
pub use agent::QueueMode;
pub use agent::RunHandle;
pub use agent_loop::run_loop;
pub use agent_loop::{EventSink, StreamFn};
pub use env::ExecutionEnv;
pub use harness::AgentHarness;
pub use harness::{NavigateTreeOptions, NavigateTreeResult};
pub use provider::ProviderError;
pub use provider::anthropic::AnthropicStreamFn;
pub use provider::openai::completions::CompletionsStreamFn;
pub use provider::openai::responses::ResponsesStreamFn;
pub use tool::{
    AgentTool, AgentToolResult, ExecutedToolCall, ExecutionMode, ToolContext, ToolState,
};
pub use tools::ToolRegistry;
pub use types::{AgentContext, AgentEvent, AgentLoopConfig, AgentMessage, AgentState};
