// Manox harness — merged from pi (core) + pi-extensions (ext).
//
// `core`  contains the agent loop, LLM providers, session management, and
//         built-in tools (the original `pi` crate).
// `ext`   contains in-process extensions — bash tool, subagent dispatch,
//         sandbox, prompt templates, etc. (the original `pi-extensions` crate).
//
// Re-exports are chosen so that downstream crates can use `manox_harness::X`
// instead of `manox_harness::core::X` for the most common public types.

pub mod core;
pub mod ext;

// Re-export core modules so downstream crates can use
// `manox_harness::tools::bash::BashTool` etc.
pub use core::agent;
pub use core::agent_loop;
pub use core::cache_stats;
pub use core::coding_agent;
pub use core::compaction;
pub use core::env;
pub use core::ext_point_agent;
pub use core::ext_point_background;
pub use core::harness;
pub use core::hashline;
pub use core::output_guard;
pub use core::provider_registry;
pub use core::session;
pub use core::settings;
pub use core::system_prompt;
pub use core::tool;
pub use core::tools;
pub use core::trust;
pub use core::types;

// Re-export ext modules (except `provider` which conflicts with core::provider).
// We explicitly re-export each module so `manox_harness::bash::BashTool` etc.
// remain reachable without the `ext::` prefix.
pub use ext::agents;
pub use ext::bash;
pub use ext::model_ref;
pub use ext::monitor;
pub use ext::path_selector;
pub use ext::prompt;
pub use ext::read;
pub use ext::sandbox;
pub use ext::session_meta;
pub use ext::session_stream;
pub use ext::steer_bus;

// `provider` exists in both core and ext. Route `manox_harness::provider` to
// ext::provider (the provider-registration extension) since that's what
// downstream crates need by path. core::provider (LLM provider impls) is
// still reachable via `manox_harness::core::provider`.
pub use ext::provider;

// Re-export core's type-level re-exports so `manox_harness::Agent` etc. work.
pub use core::agent::Agent;
pub use core::agent::QueueMode;
pub use core::agent::RunHandle;
pub use core::agent_loop::run_loop;
pub use core::agent_loop::{EventSink, StreamFn};
pub use core::env::ExecutionEnv;
pub use core::ext_point_agent::{AgentDef, AgentRegistry};
pub use core::ext_point_background::{BackgroundTaskRegistry, PollResult, TaskError, TaskId};
pub use core::harness::AgentHarness;
pub use core::harness::{NavigateTreeOptions, NavigateTreeResult};
pub use core::provider::ProviderError;
pub use core::provider::anthropic::AnthropicStreamFn;
pub use core::provider::openai::completions::CompletionsStreamFn;
pub use core::provider::openai::responses::ResponsesStreamFn;
pub use core::provider_registry::{
    Api, Cost, InputModality, ProviderConfig, ProviderModelConfig, ProviderRegistry,
};
pub use core::tool::{
    AgentTool, AgentToolResult, ExecutedToolCall, ExecutionMode, ToolContext, ToolState,
};
pub use core::tools::ToolRegistry;
pub use core::tools::bash::{BashExecRequest, BashOperations};
pub use core::types::{AgentContext, AgentEvent, AgentLoopConfig, AgentMessage, AgentState};

// Re-export ext's top-level `pub use` items.
pub use ext::agents::SubagentTool;
pub use ext::bash::background::{BackgroundRegistry, BashOutputTool, TaskStopTool};
pub use ext::bash::persistent::PersistentShellOperations;
pub use ext::monitor::MonitorTool;
