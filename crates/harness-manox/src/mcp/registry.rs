//! Process-global MCP registry (retired manox shape).
//!
//! The servers themselves connect in `agent::mcp::init` (called by
//! `agent::init`); this registry wraps every advertised tool in the manox
//! [`McpTool`] bridge so `tools::main_registry` can mount them. `init` is
//! idempotent against an already-populated registry and no-ops when the
//! shared core never initialized (pre-`agent::init` callers).

use std::sync::{Arc, OnceLock};

use gpui::App;

use crate::mcp::tool::McpTool;
use crate::tool::AnyAgentTool;

static REGISTRY: OnceLock<McpRegistry> = OnceLock::new();

pub struct McpRegistry {
    tools: Vec<AnyAgentTool>,
}

impl McpRegistry {
    pub fn tools(&self) -> &[AnyAgentTool] {
        &self.tools
    }
}

/// Wrap the shared core's connected servers in manox tool bridges. Call at
/// App startup, after `agent::init` (which runs the connections).
pub fn init(_cx: &mut App) {
    let registry = build_registry();
    let count = registry.tools.len();
    if count > 0 {
        tracing::info!("manox MCP bridge ready: {count} tools");
    }
    if let Err(rejected) = REGISTRY.set(registry) {
        tracing::warn!(
            "MCP registry already initialized; new registry ({} tools) rejected",
            rejected.tools.len()
        );
    }
}

fn build_registry() -> McpRegistry {
    let Some(core) = agent::mcp::try_global() else {
        return McpRegistry { tools: Vec::new() };
    };
    let mut tools = Vec::new();
    for server in core.servers() {
        for tool in &server.tools {
            tools.push(Arc::new(McpTool::new(
                server.name.clone(),
                tool.clone(),
                Arc::clone(&server.client),
            )) as AnyAgentTool);
        }
    }
    McpRegistry { tools }
}

/// Returns the global registry. Panics if `init` was not called.
pub fn global() -> &'static McpRegistry {
    REGISTRY
        .get()
        .expect("McpRegistry not initialized; call harness_manox::init first")
}

/// Non-panicking accessor for callers that may run before `init`
/// (e.g. unit tests building a `ToolRegistry` directly). Returns `None`
/// until `init` has populated the registry.
pub fn try_global() -> Option<&'static McpRegistry> {
    REGISTRY.get()
}
