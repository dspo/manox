//! `web_explore_read_dom`: read a tab's DOM (read-only).

use gpui::{App, Task};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::tool::AgentTool;
use crate::webview_host::BrowserTabId;

use super::schema;

pub struct WebExploreReadDomTool;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReadDomInput {
    /// The browser tab id returned by `web_explore_open`.
    tab_id: BrowserTabId,
    /// CSS selector; the `outerHTML` of the first matching element is returned.
    /// Omit to read the whole document's `outerHTML`.
    #[serde(default)]
    selector: Option<String>,
}

impl AgentTool for WebExploreReadDomTool {
    fn name(&self) -> &str {
        "WebExploreReadDom"
    }
    fn description(&self) -> &str {
        "Read the `outerHTML` of the first element matching `selector`, or the whole \
         document's `outerHTML` when `selector` is omitted. Read-only — no approval \
         needed."
    }
    fn input_schema(&self) -> serde_json::Value {
        schema::<ReadDomInput>()
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
        let parsed = match serde_json::from_value::<ReadDomInput>(input) {
            Ok(p) => p,
            Err(e) => return Task::ready(Err(format!("input parse failed: {e}"))),
        };
        let Some(host) = crate::webview_host::host() else {
            return Task::ready(Err("browser host not available".to_string()));
        };
        host.read_dom(parsed.tab_id, parsed.selector, cx)
    }
}
