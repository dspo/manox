// Pi agent harness — Rust port of the Pi coding agent's core loop, compaction,
// session management, and built-in tools.
//
// Layers (bottom-up):
//   loop.rs  — run_loop: the pure dual-loop state machine
//   agent.rs — Agent: stateful wrapper with event subscription and queues
//   harness  — AgentHarness: orchestration layer with session persistence and hooks
//
// The two external abstractions are:
//   StreamFn      — how the harness calls an LLM (provider-agnostic)
//   ExecutionEnv  — how the harness accesses filesystem and shell

pub mod cache_stats;
pub mod compaction;
pub mod env;
pub mod harness;
pub mod output_guard;
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
pub use agent_loop::run_loop;
pub use agent_loop::{StreamFn, EventSink};
pub use harness::AgentHarness;
pub use types::{AgentMessage, AgentEvent, AgentContext, AgentLoopConfig, AgentState};
pub use env::ExecutionEnv;
pub use tool::{AgentTool, AgentToolResult, ExecutedToolCall, ExecutionMode, ToolContext};
pub use tools::ToolRegistry;