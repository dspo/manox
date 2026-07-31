// Tool trait and execution pipeline.
//
// Every tool the agent can call implements `AgentTool`. The harness owns the
// execution pipeline: prepare → validate → before_hook → execute → after_hook
// → finalize. Tools only need to implement `execute`.

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::hashline::SnapshotStore;
use crate::tools::file_mutation_queue::FileMutationQueue;
use crate::types::{AgentEvent, AgentLoopConfig, ContentBlock, EventSink};

// ── Tool state ─────────────────────────────────────────────────────────────

/// Session-scoped tool state: hashline snapshots and the file mutation queue.
///
/// Carried by the harness's `ToolContext` implementation and shared by all
/// tools in a run. The snapshot store backs hashline tag validation and 3-way
/// recovery; the mutation queue serializes concurrent edits to the same file.
pub struct ToolState {
    /// Hashline snapshots keyed by path. Interior mutability is a plain
    /// `Mutex`: snapshot record/lookup never spans an `.await`.
    pub snapshots: std::sync::Mutex<SnapshotStore>,
    /// Per-file mutation locks serializing concurrent edits to the same path.
    pub mutation_queue: FileMutationQueue,
}

impl ToolState {
    pub fn new() -> Self {
        ToolState {
            snapshots: std::sync::Mutex::new(SnapshotStore::new()),
            mutation_queue: FileMutationQueue::new(),
        }
    }
}

impl Default for ToolState {
    fn default() -> Self {
        Self::new()
    }
}

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

/// Mid-execution progress channel for a tool. The loop supplies an
/// implementation that forwards each emit as a `ToolExecutionUpdate` event;
/// tools that produce incremental output (a streaming shell, a long copy)
/// report it through here. Tools with nothing to report simply never call it.
pub trait ToolProgress: Send + Sync {
    /// Report an incremental update for the running tool call.
    fn emit(&self, partial_result: JsonValue);
}

/// The result of a tool execution.
///
/// Mirrors the TS Pi `AgentToolResult`: `content` is what the model sees,
/// `details` are structured UI/log data, `usage`/`added_tool_names` carry
/// per-call token accounting when the provider reports it, and `terminate`
/// signals the loop to stop after this turn.
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
    /// Tool names this call added to the session's allowed set.
    pub added_tool_names: Option<Vec<String>>,
    /// When true, signals the agent loop to stop after this turn.
    pub terminate: bool,
}

impl AgentToolResult {
    /// Create a simple text result.
    pub fn text(text: impl Into<String>) -> Self {
        AgentToolResult {
            content: vec![ContentBlock::Text {
                text: text.into(),
                signature: None,
            }],
            details: None,
            is_error: false,
            usage: None,
            added_tool_names: None,
            terminate: false,
        }
    }

    /// Create an error result.
    pub fn error(text: impl Into<String>) -> Self {
        AgentToolResult {
            content: vec![ContentBlock::Text {
                text: text.into(),
                signature: None,
            }],
            details: None,
            is_error: true,
            usage: None,
            added_tool_names: None,
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
    /// Session-scoped tool state (hashline snapshots + file mutation queue).
    fn tool_state(&self) -> &ToolState;
}

/// Production `ToolContext` shared across an entire session.
///
/// Backs every `execute_tool_calls` invocation from the agent loop so fs/shell
/// tools reach a real `ExecutionEnv` and hashline snapshots plus the file
/// mutation queue stay coherent across turns. Cheap to clone (`Arc` bump).
pub struct LocalToolContext {
    env: std::sync::Arc<dyn crate::env::ExecutionEnv>,
    cwd: std::path::PathBuf,
    tool_state: std::sync::Arc<ToolState>,
}

impl LocalToolContext {
    pub fn new(
        env: std::sync::Arc<dyn crate::env::ExecutionEnv>,
        cwd: std::path::PathBuf,
        tool_state: std::sync::Arc<ToolState>,
    ) -> Self {
        LocalToolContext {
            env,
            cwd,
            tool_state,
        }
    }
}

impl ToolContext for LocalToolContext {
    fn env(&self) -> &dyn crate::env::ExecutionEnv {
        &*self.env
    }
    fn cwd(&self) -> &std::path::Path {
        &self.cwd
    }
    fn tool_state(&self) -> &ToolState {
        &self.tool_state
    }
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

    /// Execute with a progress reporter. Tools that emit incremental output
    /// override this and call `progress.emit(...)`; the default delegates to
    /// [`execute`](Self::execute) so tools with nothing to report are
    /// unchanged.
    async fn execute_with_progress(
        &self,
        tool_call_id: &str,
        params: JsonValue,
        signal: CancellationToken,
        ctx: &dyn ToolContext,
        progress: &dyn ToolProgress,
    ) -> Result<AgentToolResult, ToolError> {
        let _ = progress;
        self.execute(tool_call_id, params, signal, ctx).await
    }
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
/// the conversation. Each call emits a matched `ToolExecutionStart` /
/// `ToolExecutionEnd` pair and runs the optional `before_tool_call` /
/// `after_tool_call` hooks from `config`.
pub async fn execute_tool_calls(
    tool_calls: &[(&str, &str, JsonValue)],
    tools: &[Arc<dyn AgentTool>],
    signal: CancellationToken,
    ctx: &dyn ToolContext,
    config: &AgentLoopConfig,
    sink: &(dyn EventSink + Send + Sync),
    sequential: bool,
) -> (Vec<ExecutedToolCall>, Vec<crate::types::AgentMessage>) {
    // A tool that declares itself Sequential forces the whole batch to run
    // one call at a time, so its per-call ordering holds.
    let any_tool_sequential = tool_calls.iter().any(|(_, name, _)| {
        tools
            .iter()
            .find(|t| t.name() == *name)
            .is_some_and(|t| t.execution_mode() == ExecutionMode::Sequential)
    });
    let sequential = sequential || any_tool_sequential;
    if sequential {
        execute_sequential(tool_calls, tools, signal, ctx, config, sink).await
    } else {
        execute_parallel(tool_calls, tools, signal, ctx, config, sink).await
    }
}

async fn execute_sequential(
    tool_calls: &[(&str, &str, JsonValue)],
    tools: &[Arc<dyn AgentTool>],
    signal: CancellationToken,
    ctx: &dyn ToolContext,
    config: &AgentLoopConfig,
    sink: &(dyn EventSink + Send + Sync),
) -> (Vec<ExecutedToolCall>, Vec<crate::types::AgentMessage>) {
    let mut executed = Vec::with_capacity(tool_calls.len());
    let mut messages = Vec::with_capacity(tool_calls.len());

    for (id, name, args) in tool_calls {
        let outcome = execute_one((id, name, args), tools, signal.clone(), ctx, config, sink).await;
        messages.push(outcome.result_message.clone());
        executed.push(outcome);
    }

    (executed, messages)
}

async fn execute_parallel(
    tool_calls: &[(&str, &str, JsonValue)],
    tools: &[Arc<dyn AgentTool>],
    signal: CancellationToken,
    ctx: &dyn ToolContext,
    config: &AgentLoopConfig,
    sink: &(dyn EventSink + Send + Sync),
) -> (Vec<ExecutedToolCall>, Vec<crate::types::AgentMessage>) {
    let futures: Vec<_> = tool_calls
        .iter()
        .map(|(id, name, args)| {
            execute_one((id, name, args), tools, signal.clone(), ctx, config, sink)
        })
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

/// Forwards a tool's mid-execution progress reports to the loop's sink as
/// they arrive, tagged with the call's id and carrying the call's name and
/// arguments so a consumer can attach progress without cross-referencing
/// history.
///
/// The tool-facing [`ToolProgress`] callback is synchronous while the loop's
/// sink is awaited, so reports travel an unbounded channel drained by a
/// forwarding future running concurrently with execution. Awaiting the
/// forwarder once execution settles — after closing the channel — emits
/// every reported update before the call's `ToolExecutionEnd`, the same
/// ordering TS Pi's settled `updateEvents` provide, while consumers watch
/// progress in real time instead of after the fact.
struct ChannelingProgress {
    tool_call_id: String,
    tool_name: String,
    arguments: JsonValue,
    tx: tokio::sync::mpsc::UnboundedSender<AgentEvent>,
}

impl ToolProgress for ChannelingProgress {
    fn emit(&self, partial_result: JsonValue) {
        // A send failure means the forwarder is gone, which only happens
        // after execution settled — the update has nowhere left to land.
        let _ = self.tx.send(AgentEvent::ToolExecutionUpdate {
            tool_call_id: self.tool_call_id.clone(),
            tool_name: self.tool_name.clone(),
            arguments: self.arguments.clone(),
            partial_result,
        });
    }
}

async fn execute_one(
    call: (&str, &str, &JsonValue),
    tools: &[Arc<dyn AgentTool>],
    signal: CancellationToken,
    ctx: &dyn ToolContext,
    config: &AgentLoopConfig,
    sink: &(dyn EventSink + Send + Sync),
) -> ExecutedToolCall {
    let (tool_call_id, tool_name, args) = call;
    let id = tool_call_id.to_string();
    let name = tool_name.to_string();

    sink.emit(AgentEvent::ToolExecutionStart {
        tool_call_id: id.clone(),
        tool_name: name.clone(),
        arguments: args.clone(),
    })
    .await;

    // Find the tool by name.
    let tool = match tools.iter().find(|t| t.name() == tool_name) {
        Some(t) => t,
        None => {
            let result = AgentToolResult::error(format!("Tool not found: {tool_name}"));
            let result_message = make_tool_result_message(&id, &name, &result);
            sink.emit(AgentEvent::ToolExecutionEnd {
                tool_call_id: id.clone(),
                tool_name: name.clone(),
                result: result.clone(),
                is_error: result.is_error,
            })
            .await;
            return ExecutedToolCall {
                tool_call_id: id,
                tool_name: name,
                result_message,
                result,
                blocked: false,
                block_reason: None,
            };
        }
    };

    // Validate arguments against the tool's JSON Schema.
    if let Err(e) = validate_tool_args(tool.parameters_schema(), args.clone()) {
        let result = AgentToolResult::error(format!("Invalid arguments: {e}"));
        let result_message = make_tool_result_message(&id, &name, &result);
        sink.emit(AgentEvent::ToolExecutionEnd {
            tool_call_id: id.clone(),
            tool_name: name.clone(),
            result: result.clone(),
            is_error: result.is_error,
        })
        .await;
        return ExecutedToolCall {
            tool_call_id: id,
            tool_name: name,
            result_message,
            result,
            blocked: false,
            block_reason: None,
        };
    }

    // before_tool_call hook: `Some(reason)` blocks before execution.
    if let Some(before) = &config.before_tool_call
        && let Some(reason) = before(tool_call_id, tool_name, args)
    {
        let result = AgentToolResult::error(format!("blocked: {reason}"));
        let result_message = make_tool_result_message(&id, &name, &result);
        sink.emit(AgentEvent::ToolExecutionEnd {
            tool_call_id: id.clone(),
            tool_name: name.clone(),
            result: result.clone(),
            is_error: result.is_error,
        })
        .await;
        return ExecutedToolCall {
            tool_call_id: id,
            tool_name: name,
            result_message,
            result,
            blocked: true,
            block_reason: Some(reason),
        };
    }

    // Execute the tool.
    if signal.is_cancelled() {
        let result = AgentToolResult::error("aborted");
        let result_message = make_tool_result_message(&id, &name, &result);
        sink.emit(AgentEvent::ToolExecutionEnd {
            tool_call_id: id.clone(),
            tool_name: name.clone(),
            result: result.clone(),
            is_error: result.is_error,
        })
        .await;
        return ExecutedToolCall {
            tool_call_id: id,
            tool_name: name,
            result_message,
            result,
            blocked: false,
            block_reason: None,
        };
    }

    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
    let progress = ChannelingProgress {
        tool_call_id: id.clone(),
        tool_name: name.clone(),
        arguments: args.clone(),
        tx: progress_tx,
    };
    let mut forward_progress = Box::pin(async {
        while let Some(update) = progress_rx.recv().await {
            sink.emit(update).await;
        }
    });
    let mut result = {
        let execution =
            tool.execute_with_progress(tool_call_id, args.clone(), signal, ctx, &progress);
        tokio::pin!(execution);
        tokio::select! {
            outcome = &mut execution => match outcome {
                Ok(r) => r,
                Err(e) => AgentToolResult::error(format!("{e}")),
            },
            // The forwarder ends only when the sender closes, and the sender
            // lives in `progress`, borrowed by the running execution — so
            // this arm is unreachable while execution is in flight.
            _ = &mut forward_progress => AgentToolResult::error("progress forwarder stopped"),
        }
    };
    // Close the channel, then the forwarder flushes every queued update
    // before the end event.
    drop(progress);
    forward_progress.await;

    // after_tool_call hook: patches the result.
    if let Some(after) = &config.after_tool_call {
        result = after(&result);
    }

    let result_message = make_tool_result_message(&id, &name, &result);
    sink.emit(AgentEvent::ToolExecutionEnd {
        tool_call_id: id.clone(),
        tool_name: name.clone(),
        result: result.clone(),
        is_error: result.is_error,
    })
    .await;

    ExecutedToolCall {
        tool_call_id: id,
        tool_name: name,
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
        usage: result.usage.clone(),
        added_tool_names: result.added_tool_names.clone(),
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

    // A sink that discards events; pipeline tests don't assert lifecycle.
    struct NullSink;
    #[async_trait::async_trait]
    impl EventSink for NullSink {
        async fn emit(&self, _event: AgentEvent) {}
    }
    use crate::env::ExecutionEnv;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    struct MockEnv;
    struct MockCtx {
        state: ToolState,
    }

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
        async fn read_file(
            &self,
            _path: &Path,
            _offset: Option<usize>,
            _limit: Option<usize>,
        ) -> Result<String, crate::env::FileError> {
            Ok("mock content".into())
        }
        async fn write_file(
            &self,
            _path: &Path,
            _content: &str,
        ) -> Result<(), crate::env::FileError> {
            Ok(())
        }
        async fn exists(&self, _path: &Path) -> Result<bool, crate::env::FileError> {
            Ok(true)
        }
        async fn file_info(
            &self,
            _path: &Path,
        ) -> Result<crate::env::FileInfo, crate::env::FileError> {
            Ok(crate::env::FileInfo {
                path: _path.to_path_buf(),
                is_dir: false,
                size: 100,
            })
        }
        async fn list_dir(
            &self,
            _path: &Path,
        ) -> Result<Vec<crate::env::FileInfo>, crate::env::FileError> {
            Ok(vec![])
        }
        async fn create_dir(&self, _path: &Path) -> Result<(), crate::env::FileError> {
            Ok(())
        }
        async fn remove(&self, _path: &Path) -> Result<(), crate::env::FileError> {
            Ok(())
        }
        async fn exec(
            &self,
            _command: &str,
            _timeout: Duration,
            _signal: CancellationToken,
        ) -> Result<crate::env::CommandResult, crate::env::ExecutionError> {
            Ok(crate::env::CommandResult {
                stdout: "ok".into(),
                stderr: String::new(),
                exit_code: 0,
            })
        }
    }

    impl ToolContext for MockCtx {
        fn env(&self) -> &dyn ExecutionEnv {
            &MockEnv
        }
        fn cwd(&self) -> &Path {
            Path::new("/mock")
        }
        fn tool_state(&self) -> &ToolState {
            &self.state
        }
    }

    struct EchoTool;

    #[async_trait::async_trait]
    impl AgentTool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "Echoes the input"
        }
        fn parameters_schema(&self) -> JsonValue {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "message": { "type": "string" }
                },
                "required": ["message"]
            })
        }
        async fn execute(
            &self,
            _tool_call_id: &str,
            params: JsonValue,
            _signal: CancellationToken,
            _ctx: &dyn ToolContext,
        ) -> Result<AgentToolResult, ToolError> {
            let msg = params["message"].as_str().unwrap_or("no message");
            Ok(AgentToolResult::text(msg))
        }
    }

    #[tokio::test]
    async fn test_execute_single_tool() {
        let tools: Vec<Arc<dyn AgentTool>> = vec![Arc::new(EchoTool)];
        let ctx = MockCtx {
            state: ToolState::new(),
        };
        let signal = CancellationToken::new();

        let (executed, messages) = execute_tool_calls(
            &[("call_1", "echo", serde_json::json!({"message": "hello"}))],
            &tools,
            signal,
            &ctx,
            &AgentLoopConfig::default(),
            &NullSink,
            false,
        )
        .await;

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
        let tools: Vec<Arc<dyn AgentTool>> = vec![];
        let ctx = MockCtx {
            state: ToolState::new(),
        };
        let signal = CancellationToken::new();

        let (executed, _messages) = execute_tool_calls(
            &[("call_1", "nonexistent", serde_json::json!({}))],
            &tools,
            signal,
            &ctx,
            &AgentLoopConfig::default(),
            &NullSink,
            false,
        )
        .await;

        assert!(executed[0].result.is_error);
    }

    // A sink that records every emitted event for lifecycle assertions.
    struct RecordingSink(std::sync::Mutex<Vec<AgentEvent>>);
    #[async_trait::async_trait]
    impl EventSink for RecordingSink {
        async fn emit(&self, event: AgentEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    /// A tool that reports mid-execution progress before completing.
    struct ProgressTool;

    #[async_trait::async_trait]
    impl AgentTool for ProgressTool {
        fn name(&self) -> &str {
            "progress"
        }
        fn description(&self) -> &str {
            "Emits progress then returns text"
        }
        fn parameters_schema(&self) -> JsonValue {
            serde_json::json!({"type": "object", "properties": {}})
        }
        async fn execute(
            &self,
            _: &str,
            _: JsonValue,
            _: CancellationToken,
            _: &dyn ToolContext,
        ) -> Result<AgentToolResult, ToolError> {
            unreachable!("execute_with_progress must be used when present")
        }
        async fn execute_with_progress(
            &self,
            _: &str,
            _: JsonValue,
            _: CancellationToken,
            _: &dyn ToolContext,
            progress: &dyn ToolProgress,
        ) -> Result<AgentToolResult, ToolError> {
            progress.emit(serde_json::json!({"step": "halfway"}));
            Ok(AgentToolResult::text("done"))
        }
    }

    #[tokio::test]
    async fn progress_emit_surfaces_as_tool_execution_update() {
        let tools: Vec<Arc<dyn AgentTool>> = vec![Arc::new(ProgressTool)];
        let ctx = MockCtx {
            state: ToolState::new(),
        };
        let sink = RecordingSink(std::sync::Mutex::new(Vec::new()));
        let signal = CancellationToken::new();

        let (executed, _messages) = execute_tool_calls(
            &[("call_1", "progress", serde_json::json!({}))],
            &tools,
            signal,
            &ctx,
            &AgentLoopConfig::default(),
            &sink,
            false,
        )
        .await;

        let events = sink.0.lock().unwrap();
        let start = events
            .iter()
            .position(|e| matches!(e, AgentEvent::ToolExecutionStart { tool_call_id, .. } if tool_call_id == "call_1"));
        let update = events
            .iter()
            .position(|e| matches!(e, AgentEvent::ToolExecutionUpdate { tool_call_id, partial_result, .. } if tool_call_id == "call_1" && partial_result["step"] == "halfway"));
        let end = events
            .iter()
            .position(|e| matches!(e, AgentEvent::ToolExecutionEnd { tool_call_id, .. } if tool_call_id == "call_1"));
        // Start, update, end all present and in lifecycle order.
        let (start, update, end) = (
            start.expect("start"),
            update.expect("update"),
            end.expect("end"),
        );
        assert!(start < update, "update must follow start");
        assert!(update < end, "end must follow update");
        assert!(!executed[0].result.is_error);
    }

    /// A tool that emits progress, then parks on a gate until the test
    /// releases it — standing in for a long-running call.
    struct GatedProgressTool {
        release: Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl AgentTool for GatedProgressTool {
        fn name(&self) -> &str {
            "gated-progress"
        }
        fn description(&self) -> &str {
            "Emits progress then waits on a gate"
        }
        fn parameters_schema(&self) -> JsonValue {
            serde_json::json!({"type": "object", "properties": {}})
        }
        async fn execute(
            &self,
            _: &str,
            _: JsonValue,
            _: CancellationToken,
            _: &dyn ToolContext,
        ) -> Result<AgentToolResult, ToolError> {
            unreachable!("execute_with_progress must be used when present")
        }
        async fn execute_with_progress(
            &self,
            _: &str,
            _: JsonValue,
            _: CancellationToken,
            _: &dyn ToolContext,
            progress: &dyn ToolProgress,
        ) -> Result<AgentToolResult, ToolError> {
            progress.emit(serde_json::json!({"step": "before-gate"}));
            self.release.notified().await;
            Ok(AgentToolResult::text("done"))
        }
    }

    /// A sink that signals when a `ToolExecutionUpdate` lands.
    struct UpdateWatchSink {
        seen: Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl EventSink for UpdateWatchSink {
        async fn emit(&self, event: AgentEvent) {
            if matches!(event, AgentEvent::ToolExecutionUpdate { .. }) {
                self.seen.notify_one();
            }
        }
    }

    /// Progress reaches the sink while the tool is still running — not
    /// buffered until execution settles.
    #[tokio::test]
    async fn progress_streams_while_execution_is_in_flight() {
        let release = Arc::new(tokio::sync::Notify::new());
        let seen = Arc::new(tokio::sync::Notify::new());
        let tools: Vec<Arc<dyn AgentTool>> = vec![Arc::new(GatedProgressTool {
            release: Arc::clone(&release),
        })];
        let ctx = MockCtx {
            state: ToolState::new(),
        };
        let sink = UpdateWatchSink {
            seen: Arc::clone(&seen),
        };
        let signal = CancellationToken::new();
        let calls = [("call_1", "gated-progress", serde_json::json!({}))];
        let config = AgentLoopConfig::default();

        let execution = execute_tool_calls(&calls, &tools, signal, &ctx, &config, &sink, false);
        tokio::pin!(execution);

        // The update must land while the tool still waits on its gate;
        // execution settling first would mean the update was buffered.
        tokio::select! {
            _ = seen.notified() => {}
            _ = &mut execution => panic!("execution settled before its progress arrived"),
        }
        release.notify_one();
        let (executed, _) = execution.await;
        assert!(!executed[0].result.is_error);
    }

    #[tokio::test]
    async fn tool_execution_events_carry_full_payload() {
        let tools: Vec<Arc<dyn AgentTool>> = vec![Arc::new(ProgressTool)];
        let ctx = MockCtx {
            state: ToolState::new(),
        };
        let sink = RecordingSink(std::sync::Mutex::new(Vec::new()));
        let signal = CancellationToken::new();
        let args = serde_json::json!({"path": "/x"});

        let _ = execute_tool_calls(
            &[("call_1", "progress", args.clone())],
            &tools,
            signal,
            &ctx,
            &AgentLoopConfig::default(),
            &sink,
            false,
        )
        .await;

        let events = sink.0.lock().unwrap();
        let start = events.iter().find_map(|e| match e {
            AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                arguments,
            } if tool_call_id == "call_1" => Some((tool_name, arguments)),
            _ => None,
        });
        let (name, arguments) = start.expect("start payload");
        assert_eq!(name, "progress");
        assert_eq!(arguments, &args, "start must carry the call arguments");

        let update = events.iter().find_map(|e| match e {
            AgentEvent::ToolExecutionUpdate {
                tool_call_id,
                tool_name,
                arguments,
                partial_result,
            } if tool_call_id == "call_1" => Some((tool_name, arguments, partial_result)),
            _ => None,
        });
        let (name, arguments, partial_result) = update.expect("update payload");
        assert_eq!(name, "progress");
        assert_eq!(arguments, &args, "update must carry the call arguments");
        assert_eq!(partial_result["step"], "halfway");

        let end = events.iter().find_map(|e| match e {
            AgentEvent::ToolExecutionEnd {
                tool_call_id,
                tool_name,
                result,
                ..
            } if tool_call_id == "call_1" => Some((tool_name, result)),
            _ => None,
        });
        let (name, result) = end.expect("end payload");
        assert_eq!(name, "progress");
        assert!(!result.is_error, "successful call ends with is_error=false");
        assert!(
            result.content.iter().any(
                |b| matches!(b, crate::types::ContentBlock::Text { text, .. } if text == "done")
            ),
            "end must carry the final result content"
        );
    }
}
