//! `web_explore_open`: open a new browser tab navigated to a url.

use gpui::{App, Task};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::tool::AgentTool;

use super::schema;

pub struct WebExploreOpenTool;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct OpenInput {
    /// Absolute URL to navigate the new tab to (`https://` recommended).
    url: String,
}

impl AgentTool for WebExploreOpenTool {
    fn name(&self) -> &str {
        "WebExploreOpen"
    }
    fn description(&self) -> &str {
        "Open a new browser tab in the manox sidebar navigated to `url` and return its \
         numeric `tab_id` (as JSON `{\"tab_id\": N}`). Pass that id to the other \
         web_explore_* tools to drive the tab. Requires approval (subject to the \
         thread's approval mode)."
    }
    fn input_schema(&self) -> serde_json::Value {
        schema::<OpenInput>()
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
        let parsed = match serde_json::from_value::<OpenInput>(input) {
            Ok(p) => p,
            Err(e) => return Task::ready(Err(format!("input parse failed: {e}"))),
        };
        let Some(host) = crate::webview_host::host() else {
            return Task::ready(Err("browser host not available".to_string()));
        };
        match host.open_tab(&parsed.url, cx) {
            Ok(id) => Task::ready(Ok(format!("{{\"tab_id\":{id}}}"))),
            Err(e) => Task::ready(Err(e)),
        }
    }
}
