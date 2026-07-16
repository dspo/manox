//! `web_explore_click`: click an element in a tab (write).

use gpui::{App, Task};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::tool::AgentTool;
use crate::webview_host::BrowserTabId;

use super::{confirm, schema};

pub struct WebExploreClickTool;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ClickInput {
    /// The browser tab id returned by `web_explore_open`.
    tab_id: BrowserTabId,
    /// CSS selector of the element to click (first match).
    selector: String,
}

impl AgentTool for WebExploreClickTool {
    fn name(&self) -> &str {
        "web_explore_click"
    }
    fn description(&self) -> &str {
        "Click the first element matching `selector` in the tab. Requires approval \
         (subject to the thread's approval mode)."
    }
    fn input_schema(&self) -> serde_json::Value {
        schema::<ClickInput>()
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
        let parsed = match serde_json::from_value::<ClickInput>(input) {
            Ok(p) => p,
            Err(e) => return Task::ready(Err(format!("input parse failed: {e}"))),
        };
        let Some(host) = crate::webview_host::host() else {
            return Task::ready(Err("browser host not available".to_string()));
        };
        let task = host.click(parsed.tab_id, &parsed.selector, cx);
        let ok = format!("clicked `{}`", parsed.selector);
        confirm(cx, task, ok)
    }
}
