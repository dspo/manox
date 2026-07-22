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
use gpui::{App, Task};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Every LSP tool, all read-only. Empty registration is the caller's job —
/// `tools()` itself always returns the full set; the registration sites in
/// `tools/mod.rs` skip it when `try_global()` is `None`.
pub fn tools() -> Vec<AnyAgentTool> {
    let mut tools = vec![status_tool()];
    tools.extend(code_intel_tools());
    tools
}

pub fn status_tool() -> AnyAgentTool {
    Arc::new(tools::LspStatusTool) as AnyAgentTool
}

pub fn code_intel_tools() -> Vec<AnyAgentTool> {
    use tools::*;
    vec![
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

/// Start the routed server after the first successful source read. The warmup
/// is deliberately detached: the read result is never delayed by indexing.
pub(crate) fn warm_path(path: &Path, cwd: &Path) {
    let Some(registry) = try_global() else {
        return;
    };
    if registry.spec_for_path(path).is_none() {
        return;
    }
    let Some(handle) = crate::runtime::try_handle() else {
        return;
    };
    let path = path.to_path_buf();
    let cwd = cwd.to_path_buf();
    handle.spawn(async move {
        match registry.ensure_for_path(&path, &cwd).await {
            Ok(status) => tracing::info!(
                target: "manox::lsp_metrics",
                event = "automatic_warmup",
                ?status,
                path = %path.display()
            ),
            Err(error) => tracing::warn!(
                target: "manox::lsp_metrics",
                event = "automatic_warmup_failed",
                %error,
                path = %path.display()
            ),
        }
    });
}

/// Resolve the files changed by the two built-in filesystem write tools.
pub(crate) fn changed_source_paths(
    tool_name: &str,
    input: &serde_json::Value,
    cwd: &Path,
) -> Vec<PathBuf> {
    let mut paths = match tool_name {
        crate::tools::WRITE => input
            .get("path")
            .and_then(|value| value.as_str())
            .map(|path| vec![crate::tools::resolve_path(path, cwd)])
            .unwrap_or_default(),
        crate::tools::EDIT => input
            .get("patch")
            .and_then(|value| value.as_str())
            .and_then(|patch| crate::hashline::parse_patch(patch).ok())
            .map(|patches| {
                patches
                    .into_iter()
                    .map(|patch| crate::tools::resolve_path(patch.path, cwd))
                    .collect()
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    paths.sort();
    paths.dedup();
    paths
}

pub(crate) fn post_edit_diagnostics(
    paths: Vec<PathBuf>,
    cwd: PathBuf,
    cx: &mut App,
) -> Task<Result<String, String>> {
    crate::tools::bridge_tokio(cx, async move {
        let feedback = tools::diagnostics_for_paths(paths, cwd).await?;
        Ok(feedback.unwrap_or_default())
    })
}

pub(crate) fn is_semantic_tool(name: &str) -> bool {
    matches!(
        name,
        tools::GO_TO_DEFINITION
            | tools::FIND_REFERENCES
            | tools::HOVER
            | tools::DOCUMENT_SYMBOLS
            | tools::WORKSPACE_SYMBOLS
    )
}

/// Re-export the registry's process-global accessors so the rest of the agent
/// crate reaches them as `crate::lsp::try_global()` / `crate::lsp::init()` —
/// the local `pub mod lsp` would otherwise shadow the external `lsp` crate at
/// the agent root, so a bare `lsp::registry::...` there resolves to the module
/// (which has no `registry`), not the crate.
pub use lsp::registry::{LspRegistry, global, init, try_global};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changed_source_paths_resolves_write_and_multi_file_edit() {
        let cwd = Path::new("/tmp/manox-lsp-paths");
        assert_eq!(
            changed_source_paths(
                crate::tools::WRITE,
                &serde_json::json!({"path": "src/lib.rs", "content": ""}),
                cwd,
            ),
            vec![cwd.join("src/lib.rs")]
        );

        let edit = serde_json::json!({
            "patch": "[src/lib.rs#ABCD]\nINS.TAIL:\n+x\n[src/main.rs#EF01]\nINS.TAIL:\n+y"
        });
        assert_eq!(
            changed_source_paths(crate::tools::EDIT, &edit, cwd),
            vec![cwd.join("src/lib.rs"), cwd.join("src/main.rs")]
        );
    }

    #[test]
    fn semantic_tool_classification_excludes_lifecycle_and_diagnostics() {
        assert!(is_semantic_tool(tools::GO_TO_DEFINITION));
        assert!(is_semantic_tool(tools::FIND_REFERENCES));
        assert!(!is_semantic_tool(tools::LSP_STATUS));
        assert!(!is_semantic_tool(tools::DIAGNOSTICS));
    }
}
