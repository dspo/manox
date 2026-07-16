//! `web_explore_type`: type text into an element in a tab (write).
//!
//! Module named `type_text` because `type` is a reserved word.

use gpui::{App, Task};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::tool::AgentTool;
use crate::webview_host::BrowserTabId;

use super::{confirm, schema};

pub struct WebExploreTypeTool;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TypeInput {
    /// The browser tab id returned by `web_explore_open`.
    tab_id: BrowserTabId,
    /// CSS selector of the element to type into (first match).
    selector: String,
    /// Text to type into the focused element (sets value and dispatches
    /// input/change events).
    text: String,
}

impl AgentTool for WebExploreTypeTool {
    fn name(&self) -> &str {
        "web_explore_type"
    }
    fn description(&self) -> &str {
        "Focus the first element matching `selector` and type `text` into it (sets the \
         value and dispatches input/change events). Requires approval (subject to the \
         thread's approval mode)."
    }
    fn input_schema(&self) -> serde_json::Value {
        schema::<TypeInput>()
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
        let parsed = match serde_json::from_value::<TypeInput>(input) {
            Ok(p) => p,
            Err(e) => return Task::ready(Err(format!("input parse failed: {e}"))),
        };
        let Some(host) = crate::webview_host::host() else {
            return Task::ready(Err("browser host not available".to_string()));
        };
        let task = host.type_text(parsed.tab_id, &parsed.selector, &parsed.text, cx);
        let ok = format!("typed into `{}`", parsed.selector);
        confirm(cx, task, ok)
    }
}
