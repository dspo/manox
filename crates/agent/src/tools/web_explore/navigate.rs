//! `web_explore_navigate`: navigate an existing tab to a new url.

use gpui::{App, Task};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::tool::AgentTool;
use crate::webview_host::BrowserTabId;

use super::schema;

pub struct WebExploreNavigateTool;

#[derive(Deserialize, JsonSchema)]
struct NavigateInput {
    /// The browser tab id returned by `web_explore_open`.
    tab_id: BrowserTabId,
    /// Absolute URL to navigate the tab to.
    url: String,
}

impl AgentTool for WebExploreNavigateTool {
    fn name(&self) -> &str {
        "web_explore_navigate"
    }
    fn description(&self) -> &str {
        "Navigate an existing browser tab (identified by `tab_id`) to a new `url`. \
         Requires approval (subject to the thread's approval mode)."
    }
    fn input_schema(&self) -> serde_json::Value {
        schema::<NavigateInput>()
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
        let Ok(parsed) = serde_json::from_value::<NavigateInput>(input) else {
            return Task::ready(Err("input parse failed".to_string()));
        };
        let Some(host) = crate::webview_host::host() else {
            return Task::ready(Err("browser host not available".to_string()));
        };
        match host.navigate(parsed.tab_id, &parsed.url, cx) {
            Ok(()) => Task::ready(Ok(format!(
                "navigated tab {} to {}",
                parsed.tab_id, parsed.url
            ))),
            Err(e) => Task::ready(Err(e)),
        }
    }
}
