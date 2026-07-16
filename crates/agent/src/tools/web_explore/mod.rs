//! `web_explore_*` tool set — the agent's outbound surface for driving the
//! built-in browser. Registered only on the main thread's registry (sub-agents
//! do not get these), mirroring `agent` / `monitor` / worktree tools.
//!
//! Every tool reaches the browser through [`crate::webview_host::host()`]; when
//! no host is registered (non-UI contexts, or before App startup wires it), the
//! tool returns a clean `Err` rather than panicking.
//!
//! Read tools (`read_text` / `read_dom` / `screenshot`) are approval-free and
//! `is_read_only` so plan mode exposes them. Write tools (`open` / `navigate` /
//! `click` / `type` / `scroll` / `yield` / `close`) declare `requires_approval`
//! and ride the owning thread's `ApprovalMode` — OnRequest prompts, Yolo does
//! not — so the outbound trust axis is governed by the same mode that gates
//! `bash` / `write_file`.

mod click;
mod close;
mod navigate;
mod open;
mod read_dom;
mod read_text;
mod screenshot;
mod scroll;
mod type_text;
mod yield_to_user;

use std::sync::Arc;

use gpui::{App, AppContext as _, Task};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::tool::AnyAgentTool;
use crate::webview_host::BrowserTabId;

pub(crate) use crate::tools::schema;

pub use click::WebExploreClickTool;
pub use close::WebExploreCloseTool;
pub use navigate::WebExploreNavigateTool;
pub use open::WebExploreOpenTool;
pub use read_dom::WebExploreReadDomTool;
pub use read_text::WebExploreReadTextTool;
pub use screenshot::WebExploreScreenshotTool;
pub use scroll::WebExploreScrollTool;
pub use type_text::WebExploreTypeTool;
pub use yield_to_user::WebExploreYieldTool;

/// The full `web_explore_*` set, ready for `main_registry` to register. Main
/// thread only — `base_tools_with_policy` does not call this, so sub-agents and
/// plan-mode filtered lists never see browser tools.
pub fn all_tools() -> Vec<AnyAgentTool> {
    vec![
        Arc::new(WebExploreOpenTool) as AnyAgentTool,
        Arc::new(WebExploreNavigateTool),
        Arc::new(WebExploreReadTextTool),
        Arc::new(WebExploreReadDomTool),
        Arc::new(WebExploreClickTool),
        Arc::new(WebExploreTypeTool),
        Arc::new(WebExploreScrollTool),
        Arc::new(WebExploreScreenshotTool),
        Arc::new(WebExploreYieldTool),
        Arc::new(WebExploreCloseTool),
    ]
}

/// Input shape shared by the tools that address a tab by id alone
/// (`read_text` / `screenshot` / `yield` / `close`).
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct TabIdInput {
    /// The browser tab id returned by `web_explore_open`.
    tab_id: BrowserTabId,
}

/// Await a unit-resulting host task and surface a human-readable confirmation.
/// Write tools (`click` / `type` / `scroll` / `yield`) whose host method
/// returns `Result<(), String>` use this to turn `()` into a string the model
/// can read. The host task is a gpui `Task` (scheduler-aware waker), so awaiting
/// it from a background spawn reschedules correctly.
pub(crate) fn confirm(
    cx: &mut App,
    task: Task<Result<(), String>>,
    ok: String,
) -> Task<Result<String, String>> {
    cx.background_spawn(async move { task.await.map(|_| ok) })
}

#[cfg(test)]
mod tests {
    use super::*;

    const WRITE_TOOLS: &[&str] = &[
        "web_explore_open",
        "web_explore_navigate",
        "web_explore_click",
        "web_explore_type",
        "web_explore_scroll",
        "web_explore_yield",
        "web_explore_close",
    ];
    const READ_TOOLS: &[&str] = &[
        "web_explore_read_text",
        "web_explore_read_dom",
        "web_explore_screenshot",
    ];

    #[test]
    fn all_tools_has_ten_with_expected_names() {
        let tools = all_tools();
        assert_eq!(tools.len(), 10, "expected 10 web_explore tools");
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        for n in WRITE_TOOLS.iter().chain(READ_TOOLS.iter()) {
            assert!(names.contains(n), "missing {n}");
        }
    }

    #[test]
    fn read_tools_are_read_only_and_unapproved() {
        for t in &all_tools() {
            if READ_TOOLS.contains(&t.name()) {
                assert!(t.is_read_only(), "{} should be read-only", t.name());
                assert!(
                    !t.requires_approval(&serde_json::json!({})),
                    "{} should not require approval",
                    t.name()
                );
            }
        }
    }

    #[test]
    fn write_tools_require_approval_and_are_not_read_only() {
        for t in &all_tools() {
            if WRITE_TOOLS.contains(&t.name()) {
                assert!(!t.is_read_only(), "{} should not be read-only", t.name());
                assert!(
                    t.requires_approval(&serde_json::json!({})),
                    "{} should require approval",
                    t.name()
                );
            }
        }
    }

    #[test]
    fn input_schemas_are_objects() {
        for t in &all_tools() {
            assert_eq!(t.input_schema()["type"], "object", "{} schema", t.name());
        }
    }
}
