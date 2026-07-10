//! LSP tool adapters: wrap the `lsp` crate's client/registry as read-only
//! `AgentTool`s (lifecycle + code-intel).
//!
//! The JSON-RPC framer, client, and registry live in the `lsp` crate (pure
//! tokio, no `agent`/`gpui` dep) so they stay unit-testable and avoid a
//! dependency cycle. This module only shapes tool inputs/outputs, routes by
//! file extension, and bridges the async LSP work back to a gpui `Task`.
//!
//! All nine tools are `is_read_only` — they're registered on both the main
//! agent and sub-agents (LSP is a read-only axis, unlike MCP which stays
//! main-agent-only). When no server is on `PATH` or the registry isn't
//! initialized, `tools()` is a no-op (the agent degrades to grep/glob).

pub mod tools;

use crate::tool::AnyAgentTool;
use std::sync::Arc;

/// Every LSP tool, all read-only. Empty registration is the caller's job —
/// `tools()` itself always returns the full set; the registration sites in
/// `tools/mod.rs` skip it when `try_global()` is `None`.
pub fn tools() -> Vec<AnyAgentTool> {
    use tools::*;
    vec![
        Arc::new(LspStatusTool) as AnyAgentTool,
        Arc::new(LspEnsureTool) as AnyAgentTool,
        Arc::new(LspWaitReadyTool) as AnyAgentTool,
        Arc::new(GoToDefinitionTool) as AnyAgentTool,
        Arc::new(FindReferencesTool) as AnyAgentTool,
        Arc::new(HoverTool) as AnyAgentTool,
        Arc::new(DocumentSymbolsTool) as AnyAgentTool,
        Arc::new(WorkspaceSymbolsTool) as AnyAgentTool,
        Arc::new(DiagnosticsTool) as AnyAgentTool,
    ]
}

/// Re-export the registry's process-global accessors so the rest of the agent
/// crate reaches them as `crate::lsp::try_global()` / `crate::lsp::init()` —
/// the local `pub mod lsp` would otherwise shadow the external `lsp` crate at
/// the agent root, so a bare `lsp::registry::...` there resolves to the module
/// (which has no `registry`), not the crate.
pub use lsp::registry::{LspRegistry, global, init, try_global};
