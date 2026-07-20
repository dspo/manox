//! `web_explore_yield`: yield a tab to the user for a collaboration handshake.
//!
//! Module named `yield_to_user` because `yield` is a reserved word.

use gpui::{App, Task};
use tokio_util::sync::CancellationToken;

use crate::tool::AgentTool;

use super::{TabIdInput, confirm, schema};

pub struct WebExploreYieldTool;

impl AgentTool for WebExploreYieldTool {
    fn name(&self) -> &str {
        "WebExploreYield"
    }
    fn description(&self) -> &str {
        "Yield control of the tab to the user (e.g. for a login / captcha handshake). \
         Blocks until the user triggers the handback in the browser chrome, then \
         returns. Use this when the page needs human interaction before you can read \
         the authenticated result. Requires approval (subject to the thread's \
         approval mode)."
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
        let parsed = match serde_json::from_value::<TabIdInput>(input) {
            Ok(p) => p,
            Err(e) => return Task::ready(Err(format!("input parse failed: {e}"))),
        };
        let Some(host) = crate::webview_host::host() else {
            return Task::ready(Err("browser host not available".to_string()));
        };
        let task = host.yield_to_user(parsed.tab_id, cx);
        confirm(
            cx,
            task,
            "user resumed control; tab returned to agent".to_string(),
        )
    }
}
