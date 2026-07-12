//! `web_explore_screenshot`: return a tab's viewport DOM snapshot (read-only).
//!
//! The name is historical — the result is a structural DOM snapshot (outerHTML +
//! viewport/scroll metadata), not a pixel image. An LLM reasons over structure
//! better than pixels, and a pixel `takeSnapshot` would need a wry
//! platform-specific extension not exposed by 0.53.

use gpui::{App, Task};
use tokio_util::sync::CancellationToken;

use crate::tool::AgentTool;

use super::{TabIdInput, schema};

pub struct WebExploreScreenshotTool;

impl AgentTool for WebExploreScreenshotTool {
    fn name(&self) -> &str {
        "web_explore_screenshot"
    }
    fn description(&self) -> &str {
        "Return a DOM snapshot of the tab's visible viewport (structure + scroll/viewport \
         metadata, not a pixel image). Read-only — no approval needed."
    }
    fn input_schema(&self) -> serde_json::Value {
        schema::<TabIdInput>()
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn run(
        &self,
        input: serde_json::Value,
        _cancel: CancellationToken,
        _ctx: &dyn crate::tool::ToolContext,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let Ok(parsed) = serde_json::from_value::<TabIdInput>(input) else {
            return Task::ready(Err("input parse failed".to_string()));
        };
        let Some(host) = crate::webview_host::host() else {
            return Task::ready(Err("browser host not available".to_string()));
        };
        host.screenshot(parsed.tab_id, cx)
    }
}
