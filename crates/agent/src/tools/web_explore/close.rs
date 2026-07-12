//! `web_explore_close`: close and reclaim a browser tab (write).

use gpui::{App, Task};
use tokio_util::sync::CancellationToken;

use crate::tool::AgentTool;

use super::{TabIdInput, schema};

pub struct WebExploreCloseTool;

impl AgentTool for WebExploreCloseTool {
    fn name(&self) -> &str {
        "web_explore_close"
    }
    fn description(&self) -> &str {
        "Close and reclaim the browser tab identified by `tab_id`. Requires approval \
         (subject to the thread's approval mode)."
    }
    fn input_schema(&self) -> serde_json::Value {
        schema::<TabIdInput>()
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
        let Ok(parsed) = serde_json::from_value::<TabIdInput>(input) else {
            return Task::ready(Err("input parse failed".to_string()));
        };
        let Some(host) = crate::webview_host::host() else {
            return Task::ready(Err("browser host not available".to_string()));
        };
        match host.close_tab(parsed.tab_id, cx) {
            Ok(()) => Task::ready(Ok(format!("closed tab {}", parsed.tab_id))),
            Err(e) => Task::ready(Err(e)),
        }
    }
}
