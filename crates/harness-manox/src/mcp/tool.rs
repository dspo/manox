//! `AgentTool` adapter wrapping a remote MCP tool (retired manox bridge).
//!
//! Each `McpTool` holds the server name, the rmcp `Tool` definition, and a
//! clonable handle to the running rmcp client service (the connection core
//! lives in `agent::mcp`). `run()` calls `tools/call` on the tokio runtime
//! and flattens the returned text content into a single string.

use gpui::{App, Task};
use rmcp::model::{CallToolRequestParams, CallToolResult};
use tokio_util::sync::CancellationToken;

use crate::tool::AgentTool;

pub use agent::mcp::McpClientHandle;

pub struct McpTool {
    /// Cached `mcp_<server>_<tool>` id; returned by `name()` without leaking.
    name: String,
    /// Cached description (empty string when the server gave none).
    description: String,
    tool: rmcp::model::Tool,
    client: McpClientHandle,
}

impl McpTool {
    pub fn new(server_name: String, tool: rmcp::model::Tool, client: McpClientHandle) -> Self {
        let name = format!("mcp_{}_{}", server_name, tool.name);
        let description = tool
            .description
            .as_ref()
            .map(|c| c.to_string())
            .unwrap_or_default();
        Self {
            name,
            description,
            tool,
            client,
        }
    }

    /// Tool id surfaced to the model: `mcp_<server>_<tool>`. Underscore-
    /// separated to match manox's existing built-in naming; built-ins never
    /// start with `mcp_`, so there is no collision.
    pub fn tool_id(&self) -> String {
        self.name.clone()
    }
}

impl AgentTool for McpTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> serde_json::Value {
        // rmcp stores input_schema as Arc<JsonObject>. Convert to a Value::Object;
        // if `properties` is missing or null, insert an empty object so
        // OpenAI-style models accept the schema (properties must not be null).
        let map = (*self.tool.input_schema).clone();
        let mut value = serde_json::Value::Object(map);
        if let serde_json::Value::Object(ref mut obj) = value
            && obj.get("properties").is_none_or(serde_json::Value::is_null)
        {
            obj.insert(
                "properties".to_string(),
                serde_json::Value::Object(serde_json::Map::new()),
            );
        }
        value
    }

    fn run(
        &self,
        input: serde_json::Value,
        cancel: CancellationToken,
        _ctx: &dyn crate::tool::ToolContext,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let client = self.client.clone();
        let name = self.tool.name.clone();
        // bridge_tokio: F: Future<Output = Result<R, anyhow::Error>>, R: Display.
        // On success we return Ok(ToolOutput) whose Display is the text; when
        // the server flags is_error we return Err(anyhow) so bridge maps it to
        // Err(String) and the model sees a tool failure. The cancel token races
        // the rmcp call so a user-initiated stop reaps the turn promptly; the
        // server-side call is left to complete on its own (MCP has no in-flight
        // cancellation notification in the base spec).
        crate::tools::bridge_tokio(cx, async move {
            let mut params = CallToolRequestParams::new(name);
            if let serde_json::Value::Object(map) = input
                && !map.is_empty()
            {
                params = params.with_arguments(map);
            }
            let result: CallToolResult = tokio::select! {
                r = client.peer().call_tool(params) => {
                    r.map_err(|e| anyhow::anyhow!("MCP tools/call failed: {e}"))?
                }
                _ = cancel.cancelled() => return Err(anyhow::anyhow!("cancelled")),
            };
            let out = agent::mcp::flatten_call_tool_result(&result);
            if out.is_error {
                Err(anyhow::anyhow!("{}", out.text))
            } else {
                Ok(out.text)
            }
        })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn tool_id_format() {
        assert_eq!(
            format!("mcp_{}_{}", "filesystem", "read_file"),
            "mcp_filesystem_read_file"
        );
        assert_eq!(
            format!("mcp_{}_{}", "my-server", "create_issue"),
            "mcp_my-server_create_issue"
        );
    }
}
