//! `web_explore_scroll`: scroll a tab's page (write).

use gpui::{App, Task};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::tool::AgentTool;
use crate::webview_host::BrowserTabId;

use super::{confirm, schema};

pub struct WebExploreScrollTool;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ScrollInput {
    /// The browser tab id returned by `web_explore_open`.
    tab_id: BrowserTabId,
    /// Horizontal scroll delta in device pixels (positive = right).
    dx: i32,
    /// Vertical scroll delta in device pixels (positive = down).
    dy: i32,
}

impl AgentTool for WebExploreScrollTool {
    fn name(&self) -> &str {
        "web_explore_scroll"
    }
    fn description(&self) -> &str {
        "Scroll the tab's page by (dx, dy) device pixels (positive = right/down, \
         negative = left/up). Requires approval (subject to the thread's approval \
         mode)."
    }
    fn input_schema(&self) -> serde_json::Value {
        schema::<ScrollInput>()
    }
    fn requires_approval(&self, _input: &serde_json::Value) -> bool {
        true
    }
    fn run(
        &self,
        input: serde_json::Value,
        _cancel: CancellationToken,
        _ctx: &dyn crate::tool::ToolContext,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let parsed = match serde_json::from_value::<ScrollInput>(input) {
            Ok(p) => p,
            Err(e) => return Task::ready(Err(format!("input parse failed: {e}"))),
        };
        let Some(host) = crate::webview_host::host() else {
            return Task::ready(Err("browser host not available".to_string()));
        };
        let task = host.scroll(parsed.tab_id, parsed.dx, parsed.dy, cx);
        let ok = format!("scrolled by ({},{})", parsed.dx, parsed.dy);
        confirm(cx, task, ok)
    }
}
