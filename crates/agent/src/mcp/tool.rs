//! `AgentTool` adapter wrapping a remote MCP tool.
//!
//! Each `McpTool` holds the server name, the rmcp `Tool` definition, and a
//! clonable handle to the running rmcp client service. `run()` calls
//! `tools/call` on the tokio runtime and flattens the returned text content
//! into a single string.

use std::sync::Arc;

use gpui::{App, Task};
use rmcp::model::{CallToolRequestParams, CallToolResult, RawContent};
use rmcp::service::{RoleClient, RunningService};
use tokio_util::sync::CancellationToken;

use crate::tool::AgentTool;

/// Clonable handle to a running rmcp client. `RunningService` is cheaply
/// clonable (it wraps an `Arc`), so every tool from one server shares it.
pub type McpClientHandle = Arc<RunningService<RoleClient, rmcp::model::ClientInfo>>;

pub struct McpTool {
    server_name: String,
    tool: rmcp::model::Tool,
    client: McpClientHandle,
}

impl McpTool {
    pub fn new(server_name: String, tool: rmcp::model::Tool, client: McpClientHandle) -> Self {
        Self {
            server_name,
            tool,
            client,
        }
    }

    /// Tool id surfaced to the model: `mcp_<server>_<tool>`. Underscore-
    /// separated to match manox's existing built-in naming; built-ins never
    /// start with `mcp_`, so there is no collision.
    pub fn tool_id(&self) -> String {
        format!("mcp_{}_{}", self.server_name, self.tool.name)
    }
}

impl AgentTool for McpTool {
    fn name(&self) -> &str {
        Box::leak(self.tool_id().into_boxed_str())
    }

    fn description(&self) -> &str {
        match &self.tool.description {
            Some(cow) => Box::leak(cow.as_ref().to_string().into_boxed_str()),
            None => "",
        }
    }

    fn input_schema(&self) -> serde_json::Value {
        // rmcp stores input_schema as Arc<JsonObject>. Convert to a Value::Object;
        // if `properties` is missing or null, insert an empty object so
        // OpenAI-style models accept the schema (mirrors codex's mcp_tool.rs).
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
        _cancel: CancellationToken,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let client = self.client.clone();
        let name = self.tool.name.clone();
        // bridge_tokio: F: Future<Output = Result<R, anyhow::Error>>, R: Display.
        // On success we return Ok(ToolOutput) whose Display is the text; when
        // the server flags is_error we return Err(anyhow) so bridge maps it to
        // Err(String) and the model sees a tool failure.
        crate::tools::bridge_tokio(cx, async move {
            let mut params = CallToolRequestParams::new(name);
            if let serde_json::Value::Object(map) = input
                && !map.is_empty()
            {
                params = params.with_arguments(map);
            }
            let result: CallToolResult = client
                .peer()
                .call_tool(params)
                .await
                .map_err(|e| anyhow::anyhow!("MCP tools/call failed: {e}"))?;
            let out = flatten_call_tool_result(&result);
            if out.is_error {
                Err(anyhow::anyhow!("{}", out.text))
            } else {
                Ok(out)
            }
        })
    }
}

/// Concatenated text content from a tool result. `Display` yields the text.
struct ToolOutput {
    text: String,
    is_error: bool,
}

impl std::fmt::Display for ToolOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

/// Concatenate all text content blocks into a single string. Non-text blocks
/// (image/audio/resource) are skipped with a warn.
fn flatten_call_tool_result(result: &CallToolResult) -> ToolOutput {
    let mut text = String::new();
    for content in &result.content {
        let raw: &RawContent = &content.raw;
        match raw {
            RawContent::Text(t) => {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&t.text);
            }
            RawContent::Image(_) => {
                tracing::warn!("skipping MCP image content block in tool result");
            }
            RawContent::Audio(_) => {
                tracing::warn!("skipping MCP audio content block in tool result");
            }
            RawContent::Resource(_) | RawContent::ResourceLink(_) => {
                tracing::warn!("skipping MCP resource content block in tool result");
            }
        }
    }
    ToolOutput {
        text,
        is_error: result.is_error.unwrap_or(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::{CallToolResult, Content, RawContent, RawTextContent};

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

    #[test]
    fn flatten_text_blocks() {
        let result = CallToolResult::success(vec![
            Content::new(
                RawContent::Text(RawTextContent {
                    text: "hello".into(),
                    meta: None,
                }),
                None,
            ),
            Content::new(
                RawContent::Text(RawTextContent {
                    text: "world".into(),
                    meta: None,
                }),
                None,
            ),
        ]);
        let out = flatten_call_tool_result(&result);
        assert_eq!(out.text, "hello\nworld");
        assert!(!out.is_error);
    }

    #[test]
    fn flatten_error_result() {
        let result = CallToolResult::error(vec![Content::new(
            RawContent::Text(RawTextContent {
                text: "boom".into(),
                meta: None,
            }),
            None,
        )]);
        let out = flatten_call_tool_result(&result);
        assert!(out.is_error);
        assert_eq!(out.text, "boom");
    }
}
