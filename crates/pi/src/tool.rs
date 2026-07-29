// Tool trait and execution pipeline.
//
// Every tool the agent can call implements `AgentTool`. The harness owns the
// execution pipeline: prepare → validate → before_hook → execute → after_hook
// → finalize. Tools only need to implement `execute`.

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use tokio_util::sync::CancellationToken;

use crate::types::ContentBlock;

// ── Tool trait ──────────────────────────────────────────────────────────────

/// Execution mode for a batch of tool calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    /// Execute tool calls concurrently (default).
    Parallel,
    /// Execute tool calls one at a time.
    Sequential,
}

/// The result of a tool execution.
///
/// The `content` is what gets sent back to the LLM. The `details` are
/// structured data for the UI or logs.
#[derive(Debug, Clone)]
pub struct AgentToolResult {
    /// Content blocks to send back to the LLM.
    pub content: Vec<ContentBlock>,
    /// Structured details for the UI or logs.
    pub details: Option<JsonValue>,
    /// Whether this result is an error.
    pub is_error: bool,
    /// Token usage incurred by the tool itself.
    pub usage: Option<crate::types::Usage>,
    /// When true, signals the agent loop to stop after this turn.
    pub terminate: bool,
}

impl AgentToolResult {
    /// Create a simple text result.
    pub fn text(text: impl Into<String>) -> Self {
        AgentToolResult {
            content: vec![ContentBlock::Text { text: text.into(), signature: None }],
            details: None,
            is_error: false,
            usage: None,
            terminate: false,
        }
    }

    /// Create an error result.
    pub fn error(text: impl Into<String>) -> Self {
        AgentToolResult {
            content: vec![ContentBlock::Text { text: text.into(), signature: None }],
            details: None,
            is_error: true,
            usage: None,
            terminate: false,
        }
    }
}

/// Context passed to a tool during execution.
///
/// This is deliberately minimal — tools get the environment they need to
/// operate without being coupled to the harness internals.
pub trait ToolContext: Send + Sync {
    /// The execution environment (filesystem + shell).
    fn env(&self) -> &dyn crate::env::ExecutionEnv;
    /// The current working directory.
    fn cwd(&self) -> &std::path::Path;
}

/// A tool that the agent can invoke.
///
/// Every tool has a name, description, JSON Schema for its parameters, and
/// an `execute` method. The harness handles validation, hooks, and lifecycle
/// events; the tool only needs to do its job.
#[async_trait::async_trait]
pub trait AgentTool: Send + Sync {
    /// Unique tool name as exposed to the LLM.
    fn name(&self) -> &str;

    /// Human-readable description for the LLM.
    fn description(&self) -> &str;

    /// JSON Schema for the tool's parameters.
    fn parameters_schema(&self) -> JsonValue;

    /// Whether this tool requires user approval before execution.
    /// The harness calls `before_tool_call` hooks regardless of this value;
    /// this is a declarative hint for the UI.
    fn requires_approval(&self, _params: &JsonValue) -> bool {
        false
    }

    /// Whether the tool is read-only (no side effects).
    fn is_read_only(&self) -> bool {
        false
    }

    /// Preferred execution mode for this tool.
    /// When any tool in a batch returns `Sequential`, the entire batch runs
    /// sequentially.
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Parallel
    }

    /// Execute the tool with the given parameters.
    ///
    /// `signal` is cancelled when the user aborts the agent run. The tool
    /// should stop as soon as possible after cancellation.
    async fn execute(
        &self,
        tool_call_id: &str,
        params: JsonValue,
        signal: CancellationToken,
        ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError>;
}

// ── Tool error ──────────────────────────────────────────────────────────────

/// Errors that a tool can return.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("tool not found: {0}")]
    NotFound(String),
    #[error("invalid arguments: {0}")]
    InvalidArguments(String),
    #[error("execution failed: {0}")]
    ExecutionFailed(String),
    #[error("aborted")]
    Aborted,
    #[error("{0}")]
    Other(String),
}

// ── Execution pipeline ──────────────────────────────────────────────────────

/// The outcome of a single tool call after passing through the pipeline.
#[derive(Debug)]
pub struct ExecutedToolCall {
    pub tool_call_id: String,
    pub tool_name: String,
    pub result: AgentToolResult,
    pub result_message: crate::types::AgentMessage,
    /// True when the tool was blocked by a hook.
    pub blocked: bool,
    /// The block reason, if blocked.
    pub block_reason: Option<String>,
}

/// Execute a batch of tool calls using the full pipeline.
///
/// Returns the executed calls and the combined result messages to append to
/// the conversation.
pub async fn execute_tool_calls(
    tool_calls: &[(&str, &str, JsonValue)],
    tools: &[Box<dyn AgentTool>],
    signal: CancellationToken,
    ctx: &dyn ToolContext,
    sequential: bool,
) -> (Vec<ExecutedToolCall>, Vec<crate::types::AgentMessage>) {
    if sequential {
        execute_sequential(tool_calls, tools, signal, ctx).await
    } else {
        execute_parallel(tool_calls, tools, signal, ctx).await
    }
}

async fn execute_sequential(
    tool_calls: &[(&str, &str, JsonValue)],
    tools: &[Box<dyn AgentTool>],
    signal: CancellationToken,
    ctx: &dyn ToolContext,
) -> (Vec<ExecutedToolCall>, Vec<crate::types::AgentMessage>) {
    let mut executed = Vec::with_capacity(tool_calls.len());
    let mut messages = Vec::with_capacity(tool_calls.len());

    for (id, name, args) in tool_calls {
        let outcome = execute_one(id, name, args, tools, signal.clone(), ctx).await;
        messages.push(outcome.result_message.clone());
        executed.push(outcome);
    }

    (executed, messages)
}

async fn execute_parallel(
    tool_calls: &[(&str, &str, JsonValue)],
    tools: &[Box<dyn AgentTool>],
    signal: CancellationToken,
    ctx: &dyn ToolContext,
) -> (Vec<ExecutedToolCall>, Vec<crate::types::AgentMessage>) {
    let futures: Vec<_> = tool_calls
        .iter()
        .map(|(id, name, args)| execute_one(id, name, args, tools, signal.clone(), ctx))
        .collect();

    let outcomes = futures::future::join_all(futures).await;

    let mut executed = Vec::with_capacity(outcomes.len());
    let mut messages = Vec::with_capacity(outcomes.len());

    for outcome in outcomes {
        messages.push(outcome.result_message.clone());
        executed.push(outcome);
    }

    (executed, messages)
}

async fn execute_one(
    tool_call_id: &str,
    tool_name: &str,
    args: &JsonValue,
    tools: &[Box<dyn AgentTool>],
    signal: CancellationToken,
    ctx: &dyn ToolContext,
) -> ExecutedToolCall {
    // Find the tool by name.
    let tool = tools.iter().find(|t| t.name() == tool_name);

    let tool = match tool {
        Some(t) => t,
        None => {
            let result = AgentToolResult::error(format!("Tool not found: {tool_name}"));
            return ExecutedToolCall {
                tool_call_id: tool_call_id.to_string(),
                tool_name: tool_name.to_string(),
                result_message: make_tool_result_message(tool_call_id, tool_name, &result),
                result,
                blocked: false,
                block_reason: None,
            };
        }
    };

    // Validate arguments against the tool's JSON Schema.
    if let Err(e) = validate_tool_args(tool.parameters_schema(), args.clone()) {
        let result = AgentToolResult::error(format!("Invalid arguments: {e}"));
        return ExecutedToolCall {
            tool_call_id: tool_call_id.to_string(),
            tool_name: tool_name.to_string(),
            result_message: make_tool_result_message(tool_call_id, tool_name, &result),
            result,
            blocked: false,
            block_reason: None,
        };
    }

    // Execute the tool.
    if signal.is_cancelled() {
        let result = AgentToolResult::error("aborted");
        return ExecutedToolCall {
            tool_call_id: tool_call_id.to_string(),
            tool_name: tool_name.to_string(),
            result_message: make_tool_result_message(tool_call_id, tool_name, &result),
            result,
            blocked: false,
            block_reason: None,
        };
    }

    let result = match tool.execute(tool_call_id, args.clone(), signal, ctx).await {
        Ok(r) => r,
        Err(e) => AgentToolResult::error(format!("{e}")),
    };

    let result_message = make_tool_result_message(tool_call_id, tool_name, &result);

    ExecutedToolCall {
        tool_call_id: tool_call_id.to_string(),
        tool_name: tool_name.to_string(),
        result_message,
        result,
        blocked: false,
        block_reason: None,
    }
}

fn make_tool_result_message(
    tool_call_id: &str,
    tool_name: &str,
    result: &AgentToolResult,
) -> crate::types::AgentMessage {
    crate::types::AgentMessage::ToolResult {
        tool_call_id: tool_call_id.to_string(),
        tool_name: tool_name.to_string(),
        content: result.content.clone(),
        is_error: result.is_error,
        details: result.details.clone(),
        timestamp: chrono::Utc::now(),
    }
}

fn validate_tool_args(schema: JsonValue, args: JsonValue) -> Result<(), String> {
    let compiled = jsonschema::JSONSchema::compile(&schema)
        .map_err(|e| format!("schema compilation error: {e}"))?;
    let validation = compiled.validate(&args);
    if let Err(errors) = validation {
        let messages: Vec<String> = errors.map(|e| e.to_string()).collect();
        return Err(messages.join("; "));
    }
    Ok(())
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::ExecutionEnv;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    struct MockEnv;
    struct MockCtx;

    #[async_trait::async_trait]
    impl ExecutionEnv for MockEnv {
        fn cwd(&self) -> &Path {
            Path::new("/mock")
        }
        fn join_path(&self, parts: &[&str]) -> PathBuf {
            parts.iter().collect()
        }
        async fn absolute_path(&self, path: &Path) -> Result<PathBuf, crate::env::FileError> {
            Ok(path.to_path_buf())
        }
        async fn read_file(&self, _path: &Path, _offset: Option<usize>, _limit: Option<usize>) -> Result<String, crate::env::FileError> {
            Ok("mock content".into())
        }
        async fn write_file(&self, _path: &Path, _content: &str) -> Result<(), crate::env::FileError> {
            Ok(())
        }
        async fn exists(&self, _path: &Path) -> Result<bool, crate::env::FileError> {
            Ok(true)
        }
        async fn file_info(&self, _path: &Path) -> Result<crate::env::FileInfo, crate::env::FileError> {
            Ok(crate::env::FileInfo { path: _path.to_path_buf(), is_dir: false, size: 100 })
        }
        async fn list_dir(&self, _path: &Path) -> Result<Vec<crate::env::FileInfo>, crate::env::FileError> {
            Ok(vec![])
        }
        async fn create_dir(&self, _path: &Path) -> Result<(), crate::env::FileError> {
            Ok(())
        }
        async fn remove(&self, _path: &Path) -> Result<(), crate::env::FileError> {
            Ok(())
        }
        async fn exec(&self, _command: &str, _timeout: Duration) -> Result<crate::env::CommandResult, crate::env::ExecutionError> {
            Ok(crate::env::CommandResult { stdout: "ok".into(), stderr: String::new(), exit_code: 0 })
        }
    }

    impl ToolContext for MockCtx {
        fn env(&self) -> &dyn ExecutionEnv {
            &MockEnv
        }
        fn cwd(&self) -> &Path {
            Path::new("/mock")
        }
    }

    struct EchoTool;

    #[async_trait::async_trait]
    impl AgentTool for EchoTool {
        fn name(&self) -> &str { "echo" }
        fn description(&self) -> &str { "Echoes the input" }
        fn parameters_schema(&self) -> JsonValue {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "message": { "type": "string" }
                },
                "required": ["message"]
            })
        }
        async fn execute(&self, _tool_call_id: &str, params: JsonValue, _signal: CancellationToken, _ctx: &dyn ToolContext) -> Result<AgentToolResult, ToolError> {
            let msg = params["message"].as_str().unwrap_or("no message");
            Ok(AgentToolResult::text(msg))
        }
    }

    #[tokio::test]
    async fn test_execute_single_tool() {
        let tools: Vec<Box<dyn AgentTool>> = vec![Box::new(EchoTool)];
        let ctx = MockCtx;
        let signal = CancellationToken::new();

        let (executed, messages) = execute_tool_calls(
            &[("call_1", "echo", serde_json::json!({"message": "hello"}))],
            &tools,
            signal,
            &ctx,
            false,
        ).await;

        assert_eq!(executed.len(), 1);
        assert_eq!(messages.len(), 1);
        assert!(!executed[0].result.is_error);
        if let crate::types::AgentMessage::ToolResult { tool_name, .. } = &messages[0] {
            assert_eq!(tool_name, "echo");
        } else {
            panic!("expected ToolResult message");
        }
    }

    #[tokio::test]
    async fn test_tool_not_found() {
        let tools: Vec<Box<dyn AgentTool>> = vec![];
        let ctx = MockCtx;
        let signal = CancellationToken::new();

        let (executed, _messages) = execute_tool_calls(
            &[("call_1", "nonexistent", serde_json::json!({}))],
            &tools,
            signal,
            &ctx,
            false,
        ).await;

        assert!(executed[0].result.is_error);
    }
}