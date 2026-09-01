//! Pi `AgentTool` adapter wrapping a remote MCP tool.
//!
//! Each `PiMcpTool` holds the server name, the rmcp `Tool` definition, and a
//! clonable handle to the running rmcp client service. `execute` calls
//! `tools/call` natively on the tokio runtime (pi tools already run there —
//! no executor bridge needed, unlike the retired manox adapter).

use pi::tool::{AgentTool, AgentToolResult, ToolContext, ToolError};
use rmcp::model::CallToolRequestParams;
use tokio_util::sync::CancellationToken;

use super::McpClientHandle;

/// Sanitize a name segment for use in an MCP tool id. Follows the Claude
/// Code `mcp__<server>__<tool>` convention: any character outside
/// `[A-Za-z0-9_-]` becomes `_`, so server names containing `.`/`@`/spaces
/// cannot collide with the `__` separator.
fn sanitize_id_segment(segment: &str) -> String {
    segment
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

pub struct PiMcpTool {
    /// Cached `mcp__<server>__<tool>` id; returned by `name()` without leaking.
    name: String,
    /// Cached description (empty string when the server gave none).
    description: String,
    tool: rmcp::model::Tool,
    client: McpClientHandle,
}

impl PiMcpTool {
    pub fn new(server_name: String, tool: rmcp::model::Tool, client: McpClientHandle) -> Self {
        let name = format!(
            "mcp__{}__{}",
            sanitize_id_segment(&server_name),
            sanitize_id_segment(&tool.name)
        );
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
}

#[async_trait::async_trait]
impl AgentTool for PiMcpTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> serde_json::Value {
        // rmcp stores input_schema as Arc<JsonObject>. Convert to a
        // Value::Object; if `properties` is missing or null, insert an empty
        // object so OpenAI-style models accept the schema (properties must
        // not be null).
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

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        signal: CancellationToken,
        _ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        let mut request = CallToolRequestParams::new(self.tool.name.clone());
        if let serde_json::Value::Object(map) = params
            && !map.is_empty()
        {
            request = request.with_arguments(map);
        }
        // The cancel token races the rmcp call so a user-initiated stop
        // reaps the turn promptly; the server-side call is left to complete
        // on its own (MCP has no in-flight cancellation in the base spec).
        let result = tokio::select! {
            r = self.client.peer().call_tool(request) => r.map_err(|e| {
                ToolError::ExecutionFailed(format!("MCP tools/call failed: {e}"))
            })?,
            _ = signal.cancelled() => return Err(ToolError::Aborted),
        };
        let out = super::flatten_call_tool_result(&result);
        if out.is_error {
            Err(ToolError::ExecutionFailed(out.text))
        } else {
            Ok(AgentToolResult::text(out.text))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::sanitize_id_segment;

    #[test]
    fn tool_id_format() {
        assert_eq!(
            format!(
                "mcp__{}__{}",
                sanitize_id_segment("filesystem"),
                sanitize_id_segment("read_file")
            ),
            "mcp__filesystem__read_file"
        );
        assert_eq!(
            format!(
                "mcp__{}__{}",
                sanitize_id_segment("my-server"),
                sanitize_id_segment("create_issue")
            ),
            "mcp__my-server__create_issue"
        );
        // `.`/`@` and other specials escape to `_` (Claude Code convention).
        assert_eq!(
            format!(
                "mcp__{}__{}",
                sanitize_id_segment("my.server@prod"),
                sanitize_id_segment("create issue")
            ),
            "mcp__my_server_prod__create_issue"
        );
    }
}
