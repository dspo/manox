//! `web_explore_read_text`: read a tab's main text content (read-only).

use gpui::{App, Task};
use tokio_util::sync::CancellationToken;

use crate::tool::AgentTool;

use super::{TabIdInput, schema};

pub struct WebExploreReadTextTool;

impl AgentTool for WebExploreReadTextTool {
    fn name(&self) -> &str {
        "web_explore_read_text"
    }
    fn description(&self) -> &str {
        "Read the main text content of the tab's current page (readability-extracted) as \
         plain text. Read-only — no approval needed."
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
        let parsed = match serde_json::from_value::<TabIdInput>(input) {
            Ok(p) => p,
            Err(e) => return Task::ready(Err(format!("input parse failed: {e}"))),
        };
        let Some(host) = crate::webview_host::host() else {
            return Task::ready(Err("browser host not available".to_string()));
        };
        host.read_text(parsed.tab_id, cx)
    }
}
