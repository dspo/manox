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

/// Nudge flag bits for per-session-once nudges.
pub mod nudge {
    /// Bit 0: grep/rg/find/ls native tool preference shown.
    pub const GREP_TOOL_PREF: u8 = 1 << 0;
    /// Bit 1: long sleep (>=10s) → background suggestion shown.
    pub const SLEEP_BACKGROUND: u8 = 1 << 1;
    /// Bit 2: poll loop (while/for/until + sleep) → background/Monitor shown.
    pub const POLL_LOOP: u8 = 1 << 2;
    /// Bit 3: gh run watch / gh pr checks --watch → background shown.
    pub const GH_WATCH: u8 = 1 << 3;

    /// Check whether `flag` has been shown in this session.
    pub fn has(nudge_flags: &std::sync::Mutex<u8>, flag: u8) -> bool {
        nudge_flags.lock().map(|f| *f & flag != 0).unwrap_or(true)
    }

    /// Mark `flag` as shown (returns true if it was already set).
    pub fn mark(nudge_flags: &std::sync::Mutex<u8>, flag: u8) -> bool {
        let mut flags = nudge_flags.lock().unwrap_or_else(|e| e.into_inner());
        let already = *flags & flag != 0;
        *flags |= flag;
        already
    }
}

/// Session-scoped tool state: hashline snapshots, the file mutation queue,
/// and the hashline clipboard.
///
/// Carried by the harness's `ToolContext` implementation and shared by all
/// tools in a run. The snapshot store backs hashline tag validation and 3-way
/// recovery; the mutation queue serializes concurrent edits to the same file;
/// the clipboard holds lines captured by `CUT` ops for `PASTE` ops.
pub struct ToolState {
    /// Hashline snapshots keyed by path. Interior mutability is a plain
    /// `Mutex`: snapshot record/lookup never spans an `.await`.
    pub snapshots: std::sync::Mutex<SnapshotStore>,
    /// Per-file mutation locks serializing concurrent edits to the same path.
    pub mutation_queue: FileMutationQueue,
    /// Hashline clipboard: lines captured by `CUT` ops, available to `PASTE`.
    /// The anonymous register is batch-local; named registers persist across
    /// batches. For now, only the anonymous register is supported.
    pub clipboard: std::sync::Mutex<Vec<String>>,
    /// No-op edit loop detector per path: `(payload hash, consecutive count)`.
    /// A successful (changed) edit clears its path; an identical payload that
    /// again produces no change increments the count so the edit tool can
    /// escalate out of a spin.
    pub noop_edits: std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, (u64, u32)>>,
    /// The directory the last tool call ran in — the `sticky cwd` every path
    /// tool inherits when its call omits an explicit `cwd`. Advanced by
    /// `path_utils::resolve_effective_cwd` (and by the bash shell's
    /// post-command directory); `None` until the first tool call resolves, so
    /// resolution starts from the session cwd.
    pub sticky_cwd: std::sync::Mutex<Option<std::path::PathBuf>>,
    /// Per-session nudge flags: each bit marks a nudge type that has been shown
    /// this session, so nudges fire at most once per session. Bit 0 = grep
    /// native tool preference, bit 1 = sleep nudge, bit 2 = poll loop nudge,
    /// bit 3 = gh watch nudge.
    pub nudge_flags: std::sync::Mutex<u8>,
}

impl ToolState {
    pub fn new() -> Self {
        ToolState {
            snapshots: std::sync::Mutex::new(SnapshotStore::new()),
            mutation_queue: FileMutationQueue::new(),
            clipboard: std::sync::Mutex::new(Vec::new()),
            noop_edits: std::sync::Mutex::new(std::collections::HashMap::new()),
            sticky_cwd: std::sync::Mutex::new(None),
            nudge_flags: std::sync::Mutex::new(0),
        }
    }

    /// A tool state whose hashline snapshots persist under `snapshot_dir`, so
    /// a rebuilt store (restart / fork / worktree re-entry) rehydrates tags.
    pub fn with_snapshot_dir(snapshot_dir: std::path::PathBuf) -> Self {
        ToolState {
            snapshots: std::sync::Mutex::new(SnapshotStore::with_disk(snapshot_dir)),
            mutation_queue: FileMutationQueue::new(),
            clipboard: std::sync::Mutex::new(Vec::new()),
            noop_edits: std::sync::Mutex::new(std::collections::HashMap::new()),
            sticky_cwd: std::sync::Mutex::new(None),
            nudge_flags: std::sync::Mutex::new(0),
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
    /// The tool state as a shared handle when the context owns one — lets the
    /// harness carry the same state across tool-context rebuilds (cwd re-pin,
    /// fork). Contexts that borrow rather than own return `None`.
    fn tool_state_handle(&self) -> Option<std::sync::Arc<ToolState>> {
        None
    }
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
    fn tool_state_handle(&self) -> Option<std::sync::Arc<ToolState>> {
        Some(std::sync::Arc::clone(&self.tool_state))
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
///
/// The batch is scheduled as a DAG based on path dependencies and
/// declaration mode: read-only tools on disjoint paths run concurrently,
/// mutating tools are serialized against reads on the same path, and
/// tools that declare `ExecutionMode::Sequential` (Bash) conflict with
/// all other calls in the batch.
pub async fn execute_tool_calls(
    tool_calls: &[(&str, &str, JsonValue)],
    tools: &[Arc<dyn AgentTool>],
    signal: CancellationToken,
    ctx: &dyn ToolContext,
    config: &AgentLoopConfig,
    sink: &(dyn EventSink + Send + Sync),
    sequential: bool,
) -> Result<(Vec<ExecutedToolCall>, Vec<crate::types::AgentMessage>), anyhow::Error> {
    // When the host explicitly requests sequential, fall through to the
    // simple sequential path.
    if sequential {
        return execute_sequential(tool_calls, tools, signal, ctx, config, sink).await;
    }
    // Single call: no scheduling needed.
    if tool_calls.len() <= 1 {
        return execute_parallel(tool_calls, tools, signal, ctx, config, sink).await;
    }

    // Build metadata for each call.
    let n = tool_calls.len();
    let meta: Vec<ToolCallMeta> = tool_calls
        .iter()
        .map(|(_id, name, args)| {
            let tool = tools.iter().find(|t| t.name() == *name);
            let is_read_only = tool.is_some_and(|t| t.is_read_only());
            let conflicts_with_all =
                tool.is_some_and(|t| t.execution_mode() == ExecutionMode::Sequential);
            let paths = extract_call_paths(name, args);
            ToolCallMeta {
                is_read_only,
                conflicts_with_all,
                paths,
            }
        })
        .collect();

    // Build adjacency: adj[i] = list of successors of i.
    // in_degree[i] = number of predecessors.
    let mut adj: Vec<Vec<usize>> = vec![vec![]; n];
    let mut in_degree: Vec<usize> = vec![0; n];

    for i in 0..n {
        for j in (i + 1)..n {
            if should_depend(&meta[i], &meta[j]) {
                adj[i].push(j);
                in_degree[j] += 1;
            }
        }
    }

    // Iterative Kahn: each round executes all ready tasks concurrently.
    let mut results: Vec<Option<ExecutedToolCall>> = (0..n).map(|_| None).collect();

    while results.iter().any(|r| r.is_none()) {
        // Collect indices with in_degree == 0.
        let ready: Vec<usize> = (0..n)
            .filter(|i| results[*i].is_none() && in_degree[*i] == 0)
            .collect();

        if ready.is_empty() {
            // Cycle detected — should not happen with our DAG construction.
            // Fall back to sequential for safety.
            return execute_sequential(tool_calls, tools, signal, ctx, config, sink).await;
        }

        // Check cancellation before dispatching the next layer.
        if signal.is_cancelled() {
            // Fill all remaining slots with aborted results.
            for i in 0..n {
                if results[i].is_some() {
                    continue;
                }
                let (id, name, _) = tool_calls[i];
                let result = AgentToolResult::error("aborted");
                let result_message = make_tool_result_message(id, name, &result);
                results[i] = Some(ExecutedToolCall {
                    tool_call_id: id.to_string(),
                    tool_name: name.to_string(),
                    result_message,
                    result,
                    blocked: false,
                    block_reason: None,
                });
            }
            break;
        }

        // Execute ready tasks concurrently.
        let futures: Vec<_> = ready
            .iter()
            .map(|&i| {
                let (id, name, args) = &tool_calls[i];
                execute_one((*id, *name, args), tools, signal.clone(), ctx, config, sink)
            })
            .collect();

        let outcomes = futures::future::join_all(futures).await;
        let outcomes: Vec<ExecutedToolCall> = outcomes.into_iter().collect::<Result<_, _>>()?;

        for (idx, outcome) in ready.iter().zip(outcomes) {
            results[*idx] = Some(outcome);
            // Decrement in_degree of successors.
            for &succ in &adj[*idx] {
                in_degree[succ] = in_degree[succ].saturating_sub(1);
            }
        }
    }

    // Reconstruct in emission order.
    let mut executed = Vec::with_capacity(n);
    let mut messages = Vec::with_capacity(n);
    for r in results.into_iter().flatten() {
        messages.push(r.result_message.clone());
        executed.push(r);
    }

    Ok((executed, messages))
}

/// Parsed per-call metadata for dependency scheduling.
struct ToolCallMeta {
    is_read_only: bool,
    /// Tool declared `ExecutionMode::Sequential` (Bash-style) — conflicts
    /// with all other calls in the batch.
    conflicts_with_all: bool,
    /// Paths this call operates on. Empty means unknown (wildcard).
    paths: Vec<std::path::PathBuf>,
}

/// Whether call `i` (earlier in emission order) must complete before
/// call `j` (later) can start.
///
/// Rules (per plan F.3):
/// 1. If `j` conflicts with all (Bash), edge i -> j:
///    Bash waits for everything before it.
/// 2. If `i` conflicts with all (Bash), edge i -> j:
///    Everything after Bash waits for it.
/// 3. Both read-only: no edge (reads commute, no data dependency).
/// 4. If `i` is read-only and `j` is mutating on a shared path:
///    R(p) -> W(p): read must complete before write.
/// 5. If `i` is mutating and `j` is read-only on a shared path:
///    W(p) -> R(p): write must complete before subsequent read.
/// 6. If both are mutating on a shared path: W(p) -> W(p).
/// 7. Otherwise: no edge.
fn should_depend(a: &ToolCallMeta, b: &ToolCallMeta) -> bool {
    // Bash (or any Sequential-declaring tool) conflicts with everything.
    if b.conflicts_with_all || a.conflicts_with_all {
        return true;
    }
    // Both read-only: no edge needed.
    if a.is_read_only && b.is_read_only {
        return false;
    }
    // Neither has a known path: conservative edge (conflict).
    if a.paths.is_empty() && b.paths.is_empty() {
        return true;
    }
    // At least one has a known path: check for overlap.
    paths_overlap(&a.paths, &b.paths)
}

/// Check if two sets of paths overlap. An empty set is treated as wildcard
/// (conflicts with everything). Uses exact string comparison (conservative
/// approach per plan: no canonicalization, no prefix matching).
fn paths_overlap(a: &[std::path::PathBuf], b: &[std::path::PathBuf]) -> bool {
    if a.is_empty() || b.is_empty() {
        return true; // wildcard
    }
    a.iter().any(|pa| b.iter().any(|pb| pa == pb))
}

/// Extract file paths from a tool call's parameters for dependency scheduling.
///
/// Returns an empty vec when the path cannot be determined (treated as
/// wildcard / conflict-with-all). Uses conservative exact-string matching.
fn extract_call_paths(tool_name: &str, params: &serde_json::Value) -> Vec<std::path::PathBuf> {
    match tool_name {
        "Write" => params["path"]
            .as_str()
            .map(|p| vec![std::path::PathBuf::from(p)])
            .unwrap_or_default(),
        "Edit" => {
            // Use the hashline parser to extract file paths from the patch.
            if let Some(patch) = params["patch"].as_str()
                && let Ok(parsed) = crate::hashline::parser::parse_patch(patch)
            {
                return parsed.files.into_iter().map(|f| f.path).collect();
            }
            vec![]
        }
        // Read, Grep, Glob, Ls all take an optional `path` parameter.
        "Read" | "Grep" | "Glob" | "Ls" => params["path"]
            .as_str()
            .map(|p| vec![std::path::PathBuf::from(p)])
            .unwrap_or_default(),
        // Unknown tools: conservative wildcard.
        _ => vec![],
    }
}

async fn execute_sequential(
    tool_calls: &[(&str, &str, JsonValue)],
    tools: &[Arc<dyn AgentTool>],
    signal: CancellationToken,
    ctx: &dyn ToolContext,
    config: &AgentLoopConfig,
    sink: &(dyn EventSink + Send + Sync),
) -> Result<(Vec<ExecutedToolCall>, Vec<crate::types::AgentMessage>), anyhow::Error> {
    let mut executed = Vec::with_capacity(tool_calls.len());
    let mut messages = Vec::with_capacity(tool_calls.len());

    for (id, name, args) in tool_calls {
        let outcome =
            execute_one((id, name, args), tools, signal.clone(), ctx, config, sink).await?;
        messages.push(outcome.result_message.clone());
        executed.push(outcome);
    }

    Ok((executed, messages))
}

async fn execute_parallel(
    tool_calls: &[(&str, &str, JsonValue)],
    tools: &[Arc<dyn AgentTool>],
    signal: CancellationToken,
    ctx: &dyn ToolContext,
    config: &AgentLoopConfig,
    sink: &(dyn EventSink + Send + Sync),
) -> Result<(Vec<ExecutedToolCall>, Vec<crate::types::AgentMessage>), anyhow::Error> {
    let futures: Vec<_> = tool_calls
        .iter()
        .map(|(id, name, args)| {
            execute_one((id, name, args), tools, signal.clone(), ctx, config, sink)
        })
        .collect();

    let outcomes: Vec<Result<ExecutedToolCall, anyhow::Error>> =
        futures::future::join_all(futures).await;
    // Propagate the first sink failure; a persistence error must stop the
    // run before any later tool side effects.
    let outcomes: Vec<ExecutedToolCall> = outcomes.into_iter().collect::<Result<_, _>>()?;

    let mut executed = Vec::with_capacity(outcomes.len());
    let mut messages = Vec::with_capacity(outcomes.len());

    for outcome in outcomes {
        messages.push(outcome.result_message.clone());
        executed.push(outcome);
    }

    Ok((executed, messages))
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
) -> Result<ExecutedToolCall, anyhow::Error> {
    let (tool_call_id, tool_name, args) = call;
    let id = tool_call_id.to_string();
    let name = tool_name.to_string();

    sink.emit(AgentEvent::ToolExecutionStart {
        tool_call_id: id.clone(),
        tool_name: name.clone(),
        arguments: args.clone(),
    })
    .await?;

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
            .await?;
            return Ok(ExecutedToolCall {
                tool_call_id: id,
                tool_name: name,
                result_message,
                result,
                blocked: false,
                block_reason: None,
            });
        }
    };

    // A malformed stream can deliver a call whose arguments never parsed
    // into an object (null, a bare string). Fail it explicitly rather than
    // depend on every tool schema declaring `type: object`.
    if !args.is_object() {
        let result = AgentToolResult::error(format!(
            "Invalid arguments: arguments must be a JSON object, got {}",
            match args {
                serde_json::Value::Null => "null".to_string(),
                v => v.to_string(),
            }
        ));
        let result_message = make_tool_result_message(&id, &name, &result);
        sink.emit(AgentEvent::ToolExecutionEnd {
            tool_call_id: id.clone(),
            tool_name: name.clone(),
            result: result.clone(),
            is_error: result.is_error,
        })
        .await?;
        return Ok(ExecutedToolCall {
            tool_call_id: id,
            tool_name: name,
            result_message,
            result,
            blocked: false,
            block_reason: None,
        });
    }

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
        .await?;
        return Ok(ExecutedToolCall {
            tool_call_id: id,
            tool_name: name,
            result_message,
            result,
            blocked: false,
            block_reason: None,
        });
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
        .await?;
        return Ok(ExecutedToolCall {
            tool_call_id: id,
            tool_name: name,
            result_message,
            result,
            blocked: true,
            block_reason: Some(reason),
        });
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
        .await?;
        return Ok(ExecutedToolCall {
            tool_call_id: id,
            tool_name: name,
            result_message,
            result,
            blocked: false,
            block_reason: None,
        });
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
            // Real-time progress echoes are not persistence events; a dead
            // sink must not kill the run for one.
            let _ = sink.emit(update).await;
        }
    });
    let mut result = {
        // Cancellation is raced against the execution itself, not merely
        // checked around it: a tool that ignores its signal must not hold the
        // turn hostage, so on cancel the execution future is dropped where it
        // stands and settled as an error through the normal tail below.
        // Dropped tools owe drop-cleanup of live resources (process trees,
        // host round trips) — see the bash backends and `host_round_trip`.
        //
        // Deliberate deviation from upstream TS Pi, which is cooperative-only
        // (it checks `signal.aborted` around tool calls but never races tool
        // execution): manox's GUI has host-round-trip tools TS lacks, with a
        // hard requirement that a user cancel always settles the turn.
        // Mechanism, not policy.
        let cancel = signal.clone();
        let execution =
            tool.execute_with_progress(tool_call_id, args.clone(), signal, ctx, &progress);
        tokio::pin!(execution);
        tokio::select! {
            biased;
            outcome = &mut execution => match outcome {
                Ok(r) => r,
                Err(e) => AgentToolResult::error(format!("{e}")),
            },
            () = cancel.cancelled() => AgentToolResult::error("aborted"),
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
    .await?;

    Ok(ExecutedToolCall {
        tool_call_id: id,
        tool_name: name,
        result_message,
        result,
        blocked: false,
        block_reason: None,
    })
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
        async fn emit(&self, _event: AgentEvent) -> Result<(), anyhow::Error> {
            Ok(())
        }
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
        async fn exec_at(
            &self,
            _command: &str,
            _cwd: &Path,
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
        .await
        .unwrap();

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
        .await
        .unwrap();

        assert!(executed[0].result.is_error);
    }

    // A sink that records every emitted event for lifecycle assertions.
    struct RecordingSink(std::sync::Mutex<Vec<AgentEvent>>);
    #[async_trait::async_trait]
    impl EventSink for RecordingSink {
        async fn emit(&self, event: AgentEvent) -> Result<(), anyhow::Error> {
            self.0.lock().unwrap().push(event);
            Ok(())
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
        .await
        .unwrap();

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
        async fn emit(&self, event: AgentEvent) -> Result<(), anyhow::Error> {
            if matches!(event, AgentEvent::ToolExecutionUpdate { .. }) {
                self.seen.notify_one();
            }
            Ok(())
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
        let (executed, _) = execution.await.unwrap();
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
        .await
        .unwrap();

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

    // ── DAG scheduler tests ─────────────────────────────────────────────
    //
    // The test matrix verifies path-aware scheduling correctness:
    // 1. Same-file Read -> Edit ordering
    // 2. Different-file Write x2 parallel (no deadlock)
    // 3. Bash batch: reads before Bash concurrent, reads after Bash barrier
    // 4. Emission order determinism (results in original call order)
    // 5. Cancellation propagation (in-flight tasks settled, pending skipped)
    // 6. Empty batch / single call degeneracy

    /// A configurable fake tool for testing the DAG scheduler.
    ///
    /// Records start/end in a shared trace, and optionally waits on a barrier
    /// (to prove concurrent execution) or a gate (to hold until released).
    struct FakeTool {
        name: String,
        is_read_only: bool,
        execution_mode: ExecutionMode,
        barrier: Option<std::sync::Arc<tokio::sync::Barrier>>,
        gate: Option<std::sync::Arc<tokio::sync::Notify>>,
        trace: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        trace_id: String,
    }

    #[async_trait::async_trait]
    impl AgentTool for FakeTool {
        fn name(&self) -> &str {
            &self.name
        }
        fn description(&self) -> &str {
            "Fake tool for DAG scheduler tests"
        }
        fn is_read_only(&self) -> bool {
            self.is_read_only
        }
        fn execution_mode(&self) -> ExecutionMode {
            self.execution_mode
        }
        fn parameters_schema(&self) -> JsonValue {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "patch": { "type": "string" },
                    "command": { "type": "string" },
                    "content": { "type": "string" }
                }
            })
        }
        async fn execute(
            &self,
            _tool_call_id: &str,
            _params: JsonValue,
            _signal: CancellationToken,
            _ctx: &dyn ToolContext,
        ) -> Result<AgentToolResult, ToolError> {
            // Record start.
            self.trace
                .lock()
                .unwrap()
                .push(format!("start:{}", self.trace_id));

            // Wait on barrier if present (proves concurrent launch).
            if let Some(ref barrier) = self.barrier {
                barrier.wait().await;
            }

            // Wait on gate if present (holds until released).
            if let Some(ref gate) = self.gate {
                gate.notified().await;
            }

            // Record end.
            self.trace
                .lock()
                .unwrap()
                .push(format!("end:{}", self.trace_id));

            Ok(AgentToolResult::text(format!("done:{}", self.trace_id)))
        }
    }

    /// Helper: create a FakeTool with the given configuration.
    #[allow(dead_code)]
    fn make_fake(
        name: &str,
        is_read_only: bool,
        execution_mode: ExecutionMode,
        barrier: Option<std::sync::Arc<tokio::sync::Barrier>>,
        gate: Option<std::sync::Arc<tokio::sync::Notify>>,
        trace: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) -> std::sync::Arc<dyn AgentTool> {
        std::sync::Arc::new(FakeTool {
            name: name.to_string(),
            is_read_only,
            execution_mode,
            barrier,
            gate,
            trace,
            trace_id: name.to_string(),
        })
    }

    /// Helper: build a tool call tuple for the fake tool.
    fn call(
        id: &'static str,
        name: &'static str,
        path: Option<&'static str>,
    ) -> (&'static str, &'static str, JsonValue) {
        let mut args = serde_json::json!({});
        if let Some(p) = path {
            args["path"] = serde_json::json!(p);
        }
        (id, name, args)
    }

    /// Test 1: Same-file Read -> Edit ordering.
    ///
    /// A Read and an Edit on the same path must execute in emission order.
    /// The Read starts first; the Edit waits for it to complete.
    #[tokio::test]
    async fn test_same_file_read_edit_ordering() {
        let trace: std::sync::Arc<std::sync::Mutex<Vec<String>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let edit_gate = std::sync::Arc::new(tokio::sync::Notify::new());

        let tools: Vec<std::sync::Arc<dyn AgentTool>> = vec![
            make_fake(
                "Read",
                true,
                ExecutionMode::Parallel,
                None,
                None,
                std::sync::Arc::clone(&trace),
            ),
            make_fake(
                "Edit",
                false,
                ExecutionMode::Parallel,
                None,
                Some(std::sync::Arc::clone(&edit_gate)),
                std::sync::Arc::clone(&trace),
            ),
        ];

        let ctx = MockCtx {
            state: ToolState::new(),
        };
        let signal = CancellationToken::new();
        let config = AgentLoopConfig::default();
        let sink = RecordingSink(std::sync::Mutex::new(Vec::new()));

        // Both calls on the same path /a.
        let calls = [
            call("c1", "Read", Some("/a")),
            call("c2", "Edit", Some("/a")),
        ];

        // Spawn execution; it will block on the Edit gate.
        let execution = execute_tool_calls(&calls, &tools, signal, &ctx, &config, &sink, false);
        tokio::pin!(execution);

        // Poll the execution until it hits the Edit gate, yielding to let it
        // make progress.
        for _ in 0..10 {
            tokio::select! {
                biased;
                result = &mut execution => {
                    let (_executed, _messages) = result.unwrap();
                    // Execution completed early — this shouldn't happen since
                    // Edit is gated.
                    panic!("execution completed before Edit gate was released");
                }
                _ = tokio::task::yield_now() => {}
            }
        }

        // Read should have started (and completed) while Edit is still gated.
        {
            let t = trace.lock().unwrap();
            assert!(
                t.contains(&"start:Read".to_string()),
                "Read must have started: {:?}",
                *t
            );
            assert!(
                t.contains(&"end:Read".to_string()),
                "Read must have completed while Edit is gated: {:?}",
                *t
            );
            assert!(
                t.contains(&"start:Edit".to_string()),
                "Edit must have started (scheduled after Read): {:?}",
                *t
            );
            assert!(
                !t.contains(&"end:Edit".to_string()),
                "Edit must not complete while gated: {:?}",
                *t
            );
        }

        // Release the gate so Edit completes.
        edit_gate.notify_one();
        let (executed, _messages) = execution.await.unwrap();

        assert_eq!(executed.len(), 2);
        assert!(!executed[0].result.is_error);
        assert!(!executed[1].result.is_error);

        // Final trace: Read starts & ends before Edit ends.
        let t = trace.lock().unwrap();
        let read_start = t.iter().position(|s| s == "start:Read").unwrap();
        let read_end = t.iter().position(|s| s == "end:Read").unwrap();
        let edit_end = t.iter().position(|s| s == "end:Edit").unwrap();
        assert!(read_start < read_end, "Read start before end");
        assert!(read_end < edit_end, "Read completes before Edit ends");
    }

    /// Test 2: Different-file Write x2 parallel (no deadlock).
    ///
    /// Two writes on different paths should execute concurrently. We use a
    /// 2-party barrier: both writes must reach the barrier simultaneously,
    /// proving parallel execution. If the scheduler serialized them, the
    /// second write would never reach the barrier (deadlock).
    #[tokio::test]
    async fn test_different_file_writes_parallel() {
        let trace: std::sync::Arc<std::sync::Mutex<Vec<String>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));

        let tools: Vec<std::sync::Arc<dyn AgentTool>> = vec![
            make_fake(
                "Write",
                false,
                ExecutionMode::Parallel,
                Some(std::sync::Arc::clone(&barrier)),
                None,
                std::sync::Arc::clone(&trace),
            ),
            make_fake(
                "Write",
                false,
                ExecutionMode::Parallel,
                Some(std::sync::Arc::clone(&barrier)),
                None,
                std::sync::Arc::clone(&trace),
            ),
        ];

        let ctx = MockCtx {
            state: ToolState::new(),
        };
        let signal = CancellationToken::new();
        let config = AgentLoopConfig::default();
        let sink = RecordingSink(std::sync::Mutex::new(Vec::new()));

        // Different paths: /a and /b.
        let calls = [
            call("c1", "Write", Some("/a")),
            call("c2", "Write", Some("/b")),
        ];

        let (executed, _messages) =
            execute_tool_calls(&calls, &tools, signal, &ctx, &config, &sink, false)
                .await
                .unwrap();

        assert_eq!(executed.len(), 2);
        assert!(!executed[0].result.is_error);
        assert!(!executed[1].result.is_error);

        // Both writes completed (the barrier was satisfied -> parallel).
        let t = trace.lock().unwrap();
        assert!(
            t.contains(&"start:Write".to_string()),
            "Write must have started at least once: {:?}",
            *t
        );
        assert_eq!(
            t.iter().filter(|s| *s == "start:Write").count(),
            2,
            "Both Write instances must have started: {:?}",
            *t
        );
    }

    /// Test 3: Bash batch — reads before Bash concurrent, reads after Bash
    /// barrier.
    ///
    /// Batch: Read(/a), Read(/b), Bash, Read(/c).
    /// - Read(/a) and Read(/b) run concurrently (same layer, no dependency).
    /// - Bash waits for both reads (Bash conflicts with all).
    /// - Read(/c) waits for Bash.
    #[tokio::test]
    async fn test_bash_batch_ordering() {
        // Gate-free: the DAG guarantees the layering deterministically
        // ({ReadA, ReadB} -> Bash -> ReadC), so the FINAL trace pins the
        // ordering without any mid-execution observation.
        let trace: std::sync::Arc<std::sync::Mutex<Vec<String>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mk = |id: &'static str, name: &'static str, ro: bool| {
            std::sync::Arc::new(FakeTool {
                name: name.to_string(),
                is_read_only: ro,
                execution_mode: ExecutionMode::Parallel,
                barrier: None,
                gate: None,
                trace: std::sync::Arc::clone(&trace),
                trace_id: id.to_string(),
            })
        };
        // Distinct names: the scheduler resolves a call's tool by NAME, so
        // same-named fakes would all hit the first instance.
        let tools: Vec<std::sync::Arc<dyn AgentTool>> = vec![
            mk("ReadA", "ReadA", true),
            mk("ReadB", "ReadB", true),
            mk("Bash", "Bash", false),
            mk("ReadC", "ReadC", true),
        ];
        let ctx = MockCtx {
            state: ToolState::new(),
        };
        let calls = [
            call("c1", "ReadA", Some("/a")),
            call("c2", "ReadB", Some("/b")),
            call("c3", "Bash", None),
            call("c4", "ReadC", Some("/c")),
        ];
        let (executed, _) = execute_tool_calls(
            &calls,
            &tools,
            CancellationToken::new(),
            &ctx,
            &AgentLoopConfig::default(),
            &RecordingSink(std::sync::Mutex::new(Vec::new())),
            false,
        )
        .await
        .unwrap();
        assert_eq!(executed.len(), 4);
        let t = trace.lock().unwrap();
        let idx = |tag: &str| {
            t.iter()
                .position(|e| e == tag)
                .unwrap_or_else(|| panic!("{tag} missing: {t:?}"))
        };
        // Reads emitted BEFORE Bash must complete before Bash starts — and
        // they ran concurrently with each other (both started before either
        // ended is NOT asserted: only the barrier against Bash matters).
        assert!(idx("end:ReadA") < idx("start:Bash"), "{t:?}");
        assert!(idx("end:ReadB") < idx("start:Bash"), "{t:?}");
        // A read emitted AFTER a mutating call barriers on it.
        assert!(idx("end:Bash") < idx("start:ReadC"), "{t:?}");
    }

    /// Test 4: Emission order determinism.
    ///
    /// Results must be returned in the original call order regardless of
    /// scheduling order.
    #[tokio::test]
    async fn test_emission_order_determinism() {
        // Three mutating/read calls on one path execute in emission order;
        // results come back in emission order regardless of execution.
        let trace: std::sync::Arc<std::sync::Mutex<Vec<String>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mk = |id: &'static str, name: &'static str, ro: bool| {
            std::sync::Arc::new(FakeTool {
                name: name.to_string(),
                is_read_only: ro,
                execution_mode: ExecutionMode::Parallel,
                barrier: None,
                gate: None,
                trace: std::sync::Arc::clone(&trace),
                trace_id: id.to_string(),
            })
        };
        let tools: Vec<std::sync::Arc<dyn AgentTool>> = vec![
            mk("W1", "W1", false),
            mk("Read", "Read", true),
            mk("W2", "W2", false),
        ];
        let ctx = MockCtx {
            state: ToolState::new(),
        };
        let calls = [
            call("c1", "W1", Some("/a")),
            call("c2", "Read", Some("/a")),
            call("c3", "W2", Some("/a")),
        ];
        let (executed, messages) = execute_tool_calls(
            &calls,
            &tools,
            CancellationToken::new(),
            &ctx,
            &AgentLoopConfig::default(),
            &RecordingSink(std::sync::Mutex::new(Vec::new())),
            false,
        )
        .await
        .unwrap();
        // Results are reported in emission order (c1, c2, c3), not
        // completion order.
        let ids: Vec<&str> = executed.iter().map(|e| e.tool_call_id.as_str()).collect();
        assert_eq!(ids, vec!["c1", "c2", "c3"]);
        assert_eq!(messages.len(), 3);
        let t = trace.lock().unwrap();
        let idx = |tag: &str| {
            t.iter()
                .position(|e| e == tag)
                .unwrap_or_else(|| panic!("{tag} missing: {t:?}"))
        };
        // Same-path chain: W1 before Read before W2.
        assert!(idx("end:W1") < idx("start:Read"), "{t:?}");
        assert!(idx("end:Read") < idx("start:W2"), "{t:?}");
    }

    /// Test 5: Cancellation propagation.
    ///
    /// When cancelled mid-batch, in-flight tasks must settle (return aborted
    /// or complete naturally) and pending tasks must not start.
    #[tokio::test]
    async fn test_cancellation_propagation() {
        // A pre-cancelled signal: the scheduler aborts every call before
        // dispatch (pending calls never start). In-flight settle is
        // execute_one's own cancellation check, covered by its tests.
        let trace: std::sync::Arc<std::sync::Mutex<Vec<String>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mk = |id: &'static str| {
            std::sync::Arc::new(FakeTool {
                name: "Write".to_string(),
                is_read_only: false,
                execution_mode: ExecutionMode::Parallel,
                barrier: None,
                gate: None,
                trace: std::sync::Arc::clone(&trace),
                trace_id: id.to_string(),
            })
        };
        let tools: Vec<std::sync::Arc<dyn AgentTool>> = vec![mk("W1"), mk("W2"), mk("W3")];
        let ctx = MockCtx {
            state: ToolState::new(),
        };
        let calls = [
            call("c1", "Write", Some("/a")),
            call("c2", "Write", Some("/b")),
            call("c3", "Write", Some("/c")),
        ];
        let signal = CancellationToken::new();
        signal.cancel();
        let (executed, messages) = execute_tool_calls(
            &calls,
            &tools,
            signal,
            &ctx,
            &AgentLoopConfig::default(),
            &RecordingSink(std::sync::Mutex::new(Vec::new())),
            false,
        )
        .await
        .unwrap();
        assert_eq!(executed.len(), 3, "every slot is settled");
        assert!(
            executed.iter().all(|e| e.result.is_error),
            "all results are errors: {executed:?}"
        );
        assert_eq!(messages.len(), 3);
        assert!(
            trace.lock().unwrap().is_empty(),
            "no tool ran after cancellation"
        );
    }

    /// Test 6: Empty batch and single call degeneracy.
    #[tokio::test]
    async fn test_empty_batch_returns_empty() {
        let tools: Vec<std::sync::Arc<dyn AgentTool>> = vec![];
        let ctx = MockCtx {
            state: ToolState::new(),
        };
        let signal = CancellationToken::new();
        let config = AgentLoopConfig::default();
        let sink = RecordingSink(std::sync::Mutex::new(Vec::new()));

        let (executed, messages) =
            execute_tool_calls(&[], &tools, signal, &ctx, &config, &sink, false)
                .await
                .unwrap();

        assert!(executed.is_empty());
        assert!(messages.is_empty());
    }

    #[tokio::test]
    async fn test_single_call_works() {
        let trace: std::sync::Arc<std::sync::Mutex<Vec<String>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let tools: Vec<std::sync::Arc<dyn AgentTool>> = vec![make_fake(
            "Write",
            false,
            ExecutionMode::Parallel,
            None,
            None,
            trace,
        )];

        let ctx = MockCtx {
            state: ToolState::new(),
        };
        let signal = CancellationToken::new();
        let config = AgentLoopConfig::default();
        let sink = RecordingSink(std::sync::Mutex::new(Vec::new()));

        let calls = [call("c1", "Write", Some("/a"))];

        let (executed, messages) =
            execute_tool_calls(&calls, &tools, signal, &ctx, &config, &sink, false)
                .await
                .unwrap();

        assert_eq!(executed.len(), 1);
        assert_eq!(messages.len(), 1);
        assert!(!executed[0].result.is_error);
    }
}
