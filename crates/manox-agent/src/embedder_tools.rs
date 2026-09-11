//! Embedder-registered tools — the seam through which a hosting editor
//! (e.g. the VS Code agent host) contributes its own tools to a manox
//! session: editor context, diagnostics, selection readers.
//!
//! The registration store lives host-side (the AgentServer); this module is
//! the process-level bridge the engine consults while assembling a
//! session's tool set. In-process precursor of the protocol's
//! `RegisterSessionTools` / `InvokeClientTool` pair: the provider returns
//! tool adapters whose executions round-trip to the registering client over
//! the wire.

use std::sync::Arc;

use manox_harness::tool::AgentTool;

/// Process-level provider of the embedder's per-session tool registrations.
/// Mutable (not OnceLock) so the AgentServer installs its provider at
/// wiring time and tests can reset between cases — the same shape as
/// `crate::capability`'s provider.
pub trait EmbedderToolProvider: Send + Sync {
    /// The registered tools for one session, ready to be wrapped in the
    /// approval gate (the caller owns gating, exactly like MCP tools).
    fn tools_for(&self, session_id: &str) -> Vec<Arc<dyn AgentTool>>;
}

static PROVIDER: std::sync::Mutex<Option<Arc<dyn EmbedderToolProvider>>> =
    std::sync::Mutex::new(None);

/// Register the process-wide embedder-tool provider (host wiring).
/// Overwrites any prior registration.
pub fn set_provider(provider: Arc<dyn EmbedderToolProvider>) {
    *PROVIDER.lock().unwrap() = Some(provider);
}

/// The registered provider, or `None` in contexts with no embedder.
pub fn provider() -> Option<Arc<dyn EmbedderToolProvider>> {
    PROVIDER.lock().unwrap().clone()
}

/// Test-only: clear the provider so the next registration starts clean.
#[cfg(any(test, feature = "test-support"))]
pub fn drop_provider_for_test() {
    *PROVIDER.lock().unwrap() = None;
}

/// The model-facing name for an embedder tool: `client_<sanitized>`, the
/// same sanitizing idea as the MCP `mcp__<server>__<tool>` ids — the wire
/// name stays recognizable while never colliding with built-in tools.
pub fn client_tool_name(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("client_{sanitized}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_sanitized_and_prefixed() {
        assert_eq!(client_tool_name("get_selection"), "client_get_selection");
        assert_eq!(client_tool_name("a b/c"), "client_a_b_c");
    }

    #[test]
    fn provider_defaults_to_none_and_resets() {
        drop_provider_for_test();
        assert!(provider().is_none());
        struct Noop;
        impl EmbedderToolProvider for Noop {
            fn tools_for(&self, _session_id: &str) -> Vec<Arc<dyn AgentTool>> {
                Vec::new()
            }
        }
        set_provider(Arc::new(Noop));
        assert!(provider().is_some());
        assert!(provider().unwrap().tools_for("s").is_empty());
        drop_provider_for_test();
    }
}
