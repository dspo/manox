//! MCP server exposing the manox debug Harness over stdio.
//!
//! `manox --mcp` spawns one real gpui `Application` + `Workspace` window and,
//! on the tokio runtime, an rmcp server reading JSON-RPC over stdin/stdout.
//! The server holds only an `async_channel::Sender<McpRequest>` — never an
//! `AsyncApp` (gpui's `AsyncApp` is `!Send`) — so it is `Send + Sync` as
//! `ServerHandler` requires. Each `tools/call` is translated into an
//! `McpRequest`, sent across the channel to the gpui-side dispatcher
//! (`agent_ui::harness::bridge::spawn_dispatcher`), and the reply is awaited
//! on a `tokio::sync::oneshot`. This mirrors the provider layer's
//! `async_channel::bounded(64)` tokio→gpui bridge (`provider/anthropic.rs`).
//!
//! v1 is single-session: one Workspace per process. The `cargo test` path
//! (`harness/tests.rs`) covers the two-concurrent-Thread repro today; a
//! multi-session MCP path is a future extension.
//!
//! All tool names, descriptions, and result strings are English and
//! model-facing — they never pass through `i18n::t` (see CLAUDE.md §i18n).

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use agent::PermissionDecision;
use agent_ui::harness::bridge::{McpRequest, Reply};
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, JsonObject, ListToolsResult,
    PaginatedRequestParams,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ErrorData, ServerHandler};
use serde_json::{Value, json};
use tokio::sync::oneshot;

/// The MCP server. Holds the send-end of the dispatcher channel only.
pub struct ManoxMcpServer {
    pub tx: async_channel::Sender<McpRequest>,
}

impl ManoxMcpServer {
    pub fn new(tx: async_channel::Sender<McpRequest>) -> Self {
        Self { tx }
    }
}

impl ServerHandler for ManoxMcpServer {
    /// Advertize the fixed tool set. v1 tools map 1:1 to `McpRequest` variants.
    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, ErrorData>> + Send + '_ {
        std::future::ready(Ok(ListToolsResult {
            tools: tool_list(),
            ..Default::default()
        }))
    }

    /// Dispatch one `tools/call` to the gpui-side dispatcher and await its reply.
    fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<CallToolResult, ErrorData>> + Send + '_ {
        let tx = self.tx.clone();
        async move {
            let args = request.arguments.unwrap_or_default();
            let reply = match dispatch_call(&request.name, &args, &tx).await {
                Ok(req) => req, // already built+sent inside dispatch_call
                Err(msg) => {
                    return Ok(CallToolResult::error(vec![Content::text(msg)]));
                }
            };
            match reply.await {
                Ok(Ok(value)) => Ok(CallToolResult::success(vec![Content::text(
                    value_to_string(&value),
                )])),
                Ok(Err(err)) => Ok(CallToolResult::error(vec![Content::text(err)])),
                // The dispatcher dropped the reply sender (window closed mid-call).
                Err(_) => Ok(CallToolResult::error(vec![Content::text(
                    "dispatcher dropped the reply (window closed)",
                )])),
            }
        }
    }
}

/// Build the `McpRequest` for a tool name, send it over the channel, and
/// return the reply receiver. Argument parsing failures become user-visible
/// `Err` strings (rendered in the caller's MCP client), not protocol errors.
async fn dispatch_call(
    name: &str,
    args: &JsonObject,
    tx: &async_channel::Sender<McpRequest>,
) -> Result<oneshot::Receiver<Result<Value, String>>, String> {
    let (s, r) = oneshot::channel();
    let reply: Reply = s;
    let req = match name {
        "manox_new_thread" => McpRequest::NewThread { reply },
        "manox_open_thread" => {
            let id = get_str(args, "id")?;
            McpRequest::OpenThread { id, reply }
        }
        "manox_list_threads" => McpRequest::ListThreads { reply },
        "manox_send_message" => {
            let text = get_str(args, "text")?;
            McpRequest::SendMessage { text, reply }
        }
        "manox_send_command" => {
            let cmd_name = get_str(args, "name")?;
            let cmd_args = get_str(args, "args").unwrap_or_default();
            McpRequest::SendCommand {
                name: cmd_name,
                args: cmd_args,
                reply,
            }
        }
        "manox_approve" => {
            let decision = parse_decision(&get_str(args, "decision")?)?;
            McpRequest::Approve { decision, reply }
        }
        "manox_plan_respond" => {
            let choice = match get_str(args, "choice")?.as_str() {
                "implement" => agent::PlanReviewChoice::Implement,
                "implement_clear" => agent::PlanReviewChoice::ImplementClearContext,
                "stay" => agent::PlanReviewChoice::StayInPlan,
                other => {
                    return Err(format!(
                        "unknown plan review choice: {other} (implement | implement_clear | stay)"
                    ));
                }
            };
            McpRequest::PlanReviewRespond { choice, reply }
        }
        "manox_cancel" => McpRequest::Cancel { reply },
        "manox_read_conversation" => McpRequest::ReadConversation { reply },
        "manox_read_messages" => McpRequest::ReadMessages { reply },
        "manox_is_running" => McpRequest::IsRunning { reply },
        "manox_await_idle" => {
            let timeout_ms = get_u64(args, "timeout_ms").unwrap_or(30_000);
            McpRequest::AwaitIdle {
                timeout: Duration::from_millis(timeout_ms),
                reply,
            }
        }
        "manox_quit" => McpRequest::Quit { reply },
        other => return Err(format!("unknown tool: {other}")),
    };
    tx.send(req)
        .await
        .map_err(|_| "dispatcher channel closed (window closed)".to_string())?;
    Ok(r)
}

fn get_str(args: &JsonObject, key: &str) -> Result<String, String> {
    match args.get(key) {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(other) => Err(format!("argument `{key}` must be a string, got {other}")),
        None => Err(format!("missing required argument `{key}`")),
    }
}

fn get_u64(args: &JsonObject, key: &str) -> Result<u64, String> {
    match args.get(key) {
        Some(Value::Number(n)) => n
            .as_u64()
            .ok_or_else(|| format!("argument `{key}` must be a non-negative integer")),
        Some(other) => Err(format!("argument `{key}` must be a number, got {other}")),
        None => Err(format!("missing required argument `{key}`")),
    }
}

fn parse_decision(s: &str) -> Result<PermissionDecision, String> {
    match s {
        "once" => Ok(PermissionDecision::AllowOnce),
        "always_allow" => Ok(PermissionDecision::AlwaysAllow),
        "deny" => Ok(PermissionDecision::Deny),
        other => Err(format!(
            "decision must be one of `once`, `always_allow`, `deny`; got `{other}`"
        )),
    }
}

fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        _ => serde_json::to_string(v).unwrap_or_else(|_| v.to_string()),
    }
}

/// Build the advertised tool set. Constructed fresh per `list_tools` call;
/// `Tool` clones its `Arc<JsonObject>` schema cheaply.
fn tool_list() -> Vec<rmcp::model::Tool> {
    fn tool(name: &str, desc: &str, schema: Value) -> rmcp::model::Tool {
        rmcp::model::Tool::new(
            name.to_string(),
            desc.to_string(),
            Arc::new(schema.as_object().cloned().unwrap_or_default()),
        )
    }

    vec![
        tool(
            "manox_new_thread",
            "Start a fresh thread in the current manox session. The previous thread is persisted; the workspace switches to a blank one.",
            json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        ),
        tool(
            "manox_open_thread",
            "Open a persisted thread by id in the current session.",
            json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Thread id (from manox_list_threads)." }
                },
                "required": ["id"],
                "additionalProperties": false
            }),
        ),
        tool(
            "manox_list_threads",
            "List all persisted threads (id + title) in sidebar order.",
            json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        ),
        tool(
            "manox_send_message",
            "Send a user message and start a model turn. Refused if a turn is already running — call manox_await_idle first.",
            json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "The user message text." }
                },
                "required": ["text"],
                "additionalProperties": false
            }),
        ),
        tool(
            "manox_send_command",
            "Run a slash command (`/name args`) as a user turn.",
            json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Slash command name without the leading slash." },
                    "args": { "type": "string", "description": "Optional arguments string.", "default": "" }
                },
                "required": ["name"],
                "additionalProperties": false
            }),
        ),
        tool(
            "manox_approve",
            "Resolve the pending tool-call authorization. Call manox_read_conversation first to see what is pending.",
            json!({
                "type": "object",
                "properties": {
                    "decision": { "type": "string", "enum": ["once", "always_allow", "deny"] }
                },
                "required": ["decision"],
                "additionalProperties": false
            }),
        ),
        tool(
            "manox_plan_respond",
            "Resolve the pending plan review with the user's verdict.",
            json!({
                "type": "object",
                "properties": {
                    "choice": {
                        "type": "string",
                        "enum": ["implement", "implement_clear", "stay"]
                    }
                },
                "required": ["choice"],
                "additionalProperties": false
            }),
        ),
        tool(
            "manox_cancel",
            "Cancel the in-flight turn.",
            json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        ),
        tool(
            "manox_read_conversation",
            "Read the rendered conversation: a JSON array of items (user/assistant/reasoning/tool_call/agent_task/error/notice) with their text and status. This is the agent-facing view of what the user sees.",
            json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        ),
        tool(
            "manox_read_messages",
            "Read the canonical Thread messages (role + flattened text). This is the source-of-truth persisted state, as opposed to the rendered view.",
            json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        ),
        tool(
            "manox_is_running",
            "Whether the current thread is running a model turn.",
            json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        ),
        tool(
            "manox_await_idle",
            "Block until the current thread finishes its turn, or the timeout elapses. Returns {state: \"idle\"|\"still_running\"|\"window_gone\"}.",
            json!({
                "type": "object",
                "properties": {
                    "timeout_ms": { "type": "integer", "default": 30000, "description": "Max wait in milliseconds." }
                },
                "additionalProperties": false
            }),
        ),
        tool(
            "manox_quit",
            "Quit the manox process. Use when the debugging session is done.",
            json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        ),
    ]
}
