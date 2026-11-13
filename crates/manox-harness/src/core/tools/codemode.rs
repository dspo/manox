// Codemode tool — a JS script (an async function body) run in a QuickJS
// sandbox whose only outward capability is calling this session's other
// tools.
//
// The tool carries no authority of its own: the sibling tool instances it
// nests into are the exact wrapped instances the top-level surface mounts
// (approval gating, file locks, fences), so every nested call is judged like
// the equivalent top-level call — the sandbox adds no bypass. The bridge
// (`manox-codemode`) runs nested calls on spawned tasks, so this side owns
// two seams the bridge cannot: an owned `ToolContext` (reinstalled per
// execution from the incoming call's context) and the sequential-execution
// declaration — the top-level DAG scheduler never sees nested calls, so a
// sibling that declares `Sequential` is serialized here by a per-tool mutex.

use std::collections::BTreeMap;
use std::sync::Arc;

use manox_codemode::CodemodeExecutor;
use serde_json::Value as JsonValue;
use tokio_util::sync::CancellationToken;

use crate::tool::{
    AgentTool, AgentToolResult, ExecutionMode, LocalToolContext, ToolContext, ToolError,
};
use crate::types::ContentBlock;

/// The static prose half of the description; the dynamic half (the siblings'
/// TypeScript declarations) is appended at construction.
const PROMPT: &str = include_str!("codemode_prompt.md");

/// One sibling adapted to the bridge's tool trait. The adapter holds the
/// per-execution call environment (parent call id + owned context) in a
/// mutex cell: a `CodemodeTool` never runs two executions concurrently (it
/// declares `Sequential`), so the cell has a single writer per execution and
/// every nested call of that execution reads the same environment.
struct SiblingAdapter {
    tool: Arc<dyn AgentTool>,
    /// Present exactly when the sibling declares `ExecutionMode::Sequential`:
    /// the bridge's concurrency is invisible to the top-level scheduler, so
    /// the declaration is inherited here. The slot is SHARED across all
    /// sequential siblings — the top-level semantics of `Sequential` is
    /// "conflicts with every other call in the batch", so sequential tools
    /// serialize against each other (and against themselves), never just
    /// per instance.
    serial: Option<Arc<tokio::sync::Mutex<()>>>,
    call_env: std::sync::Mutex<CallEnv>,
}

/// The environment one execution's nested calls run under.
struct CallEnv {
    /// The top-level call id of the running execution; nested ids are
    /// `"{parent}:{script_call_id}"` so an approval card (or any log line)
    /// attributes a nested call to the script invocation that issued it.
    parent_call_id: String,
    /// Owned context rebuilt from the execution's incoming context — the
    /// bridge spawns its nested calls, so the borrowed `&dyn ToolContext`
    /// cannot cross that boundary.
    ctx: Option<Arc<dyn ToolContext>>,
}

impl SiblingAdapter {
    fn new(tool: Arc<dyn AgentTool>, serial_slot: &Arc<tokio::sync::Mutex<()>>) -> Self {
        let serial =
            (tool.execution_mode() == ExecutionMode::Sequential).then(|| Arc::clone(serial_slot));
        SiblingAdapter {
            tool,
            serial,
            call_env: std::sync::Mutex::new(CallEnv {
                parent_call_id: String::new(),
                ctx: None,
            }),
        }
    }

    fn install(&self, parent_call_id: &str, ctx: Arc<dyn ToolContext>) {
        let mut env = self.call_env.lock().unwrap_or_else(|e| e.into_inner());
        env.parent_call_id = parent_call_id.to_string();
        env.ctx = Some(ctx);
    }
}

#[async_trait::async_trait]
impl manox_codemode::CodemodeTool for SiblingAdapter {
    fn name(&self) -> &str {
        self.tool.name()
    }

    fn description(&self) -> &str {
        self.tool.description()
    }

    fn parameters_schema(&self) -> JsonValue {
        self.tool.parameters_schema()
    }

    async fn execute(
        &self,
        script_call_id: &str,
        args: JsonValue,
        signal: CancellationToken,
    ) -> Result<JsonValue, String> {
        let (parent_call_id, ctx) = {
            let env = self.call_env.lock().unwrap_or_else(|e| e.into_inner());
            match &env.ctx {
                Some(ctx) => (env.parent_call_id.clone(), Arc::clone(ctx)),
                None => {
                    return Err(
                        "the codemode tool has no execution context installed; this is a harness bug"
                            .to_string(),
                    );
                }
            }
        };
        // `:` keeps the pair machine-splittable while reading as one path
        // (`toolu_01:3`); the script-side id is the bridge's call counter.
        let nested_id = format!("{parent_call_id}:{script_call_id}");
        let run = self.tool.execute(&nested_id, args, signal, &*ctx);
        let outcome = match &self.serial {
            Some(serial) => {
                let _guard = serial.lock().await;
                run.await
            }
            None => run.await,
        };
        outcome
            .map_err(|error| error.to_string())
            .map(|result| tool_result_value(&result))
    }
}

/// The script-visible face of one nested result: the tool's text output, its
/// error flag, and its structured details, with images surfaced alongside.
/// Stable shape — scripts are written against it.
fn tool_result_value(result: &AgentToolResult) -> JsonValue {
    let mut text_parts: Vec<&str> = Vec::new();
    let mut images: Vec<JsonValue> = Vec::new();
    for block in &result.content {
        match block {
            ContentBlock::Text { text, .. } => text_parts.push(text.as_str()),
            ContentBlock::Image { data, mime_type } => images.push(serde_json::json!({
                "data": data,
                "mimeType": mime_type,
            })),
            _ => {}
        }
    }
    let mut value = serde_json::json!({
        "output": text_parts.join("\n"),
        "isError": result.is_error,
    });
    if !images.is_empty() {
        value["images"] = JsonValue::Array(images);
    }
    if let Some(details) = &result.details {
        value["details"] = details.clone();
    }
    value
}

/// The session-facing Codemode tool. Construction renders the siblings'
/// declarations once; execution runs the script and folds the store.
pub struct CodemodeTool {
    executor: Result<CodemodeExecutor, String>,
    /// The adapted siblings, kept for the per-execution context install.
    adapters: Vec<Arc<SiblingAdapter>>,
    /// In-session store continuity: the snapshot the next execution starts
    /// from, updated with each successful execution's writes. Resume
    /// continuity is separate — the journal replay folds the same writes out
    /// of the transcript at assembly (manox-agent `codemode` module).
    store: std::sync::Mutex<BTreeMap<String, String>>,
    description: String,
}

impl CodemodeTool {
    /// Build the tool over the siblings a script may call — every mounted
    /// tool EXCEPT Codemode itself (the caller assembles the list before
    /// pushing this tool, so the exclusion is structural). `initial_store`
    /// seeds the session's `store`/`load` map (the journal replay's fold; an
    /// empty map for a fresh session). The map type is the bridge's own
    /// snapshot type (key -> serialized JSON), so the replay's fold lands
    /// verbatim.
    pub fn new(
        siblings: Vec<Arc<dyn AgentTool>>,
        initial_store: std::collections::BTreeMap<String, String>,
    ) -> Self {
        let serial_slot = Arc::new(tokio::sync::Mutex::new(()));
        let adapters: Vec<Arc<SiblingAdapter>> = siblings
            .into_iter()
            .map(|tool| SiblingAdapter::new(tool, &serial_slot))
            .map(Arc::new)
            .collect();
        let bridge_tools: Vec<Arc<dyn manox_codemode::CodemodeTool>> = adapters
            .iter()
            .map(|adapter| Arc::clone(adapter) as Arc<dyn manox_codemode::CodemodeTool>)
            .collect();
        let executor = CodemodeExecutor::new(bridge_tools);
        let description = match &executor {
            Ok(executor) => {
                let rendered = executor.rendered_declarations();
                let mut description = String::from(PROMPT);
                description.push_str("\n\n## Tools callable from the script\n\n```ts\n");
                for tool in &rendered.rendered {
                    if let Some(declaration) = &tool.declaration {
                        description
                            .push_str(&format!("// {}\n  {}\n", tool.description, declaration));
                    }
                }
                description.push_str("```\n");
                if !rendered.dropped.is_empty() {
                    description.push_str(&format!(
                        "\n{} tool{} ({} too large to declare) {} still callable — discover \
                         {} with `ALL_TOOLS` / `describeTool`.\n",
                        rendered.dropped.len(),
                        if rendered.dropped.len() == 1 { "" } else { "s" },
                        rendered.dropped.join(", "),
                        if rendered.dropped.len() == 1 {
                            "is"
                        } else {
                            "are"
                        },
                        if rendered.dropped.len() == 1 {
                            "it"
                        } else {
                            "them"
                        },
                    ));
                }
                description
            }
            Err(error) => format!(
                "{PROMPT}\n\nThis Codemode instance is UNAVAILABLE: its sibling tool list is \
                 invalid ({error}). Calling it fails.\n"
            ),
        };
        CodemodeTool {
            executor,
            adapters,
            store: std::sync::Mutex::new(initial_store),
            description,
        }
    }

    /// The tool names callable from scripts, in declaration order — Codemode
    /// itself is never among them (the sibling list is assembled without it).
    pub fn script_tool_names(&self) -> Vec<String> {
        match &self.executor {
            Ok(executor) => executor
                .declared_tools()
                .iter()
                .map(|tool| tool.name.clone())
                .collect(),
            Err(_) => Vec::new(),
        }
    }
}

#[async_trait::async_trait]
impl AgentTool for CodemodeTool {
    fn name(&self) -> &str {
        "Codemode"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> JsonValue {
        serde_json::json!({
            "type": "object",
            "properties": {
                "code": {
                    "type": "string",
                    "description": "JavaScript source: an async function body. Optionally a \
                                    first line `// @options: {\"max_output_tokens\": N, \
                                    \"timeout_ms\": N}`."
                }
            },
            "required": ["code"]
        })
    }

    fn is_read_only(&self) -> bool {
        false
    }

    // Session runtime state (the script's own store map) is the only thing
    // this tool writes; every filesystem mutation happens inside a nested
    // call, which carries the authority at its own layer — same admission
    // shape as the goal tools.
    fn mutates_through_fs_paths(&self) -> bool {
        false
    }

    fn requires_approval(&self, _params: &JsonValue) -> bool {
        false
    }

    fn execution_mode(&self) -> ExecutionMode {
        // Nested calls are invisible to the top-level DAG: the only safe
        // batch position for a script is alone, strictly ordered against
        // every neighbor (same declaration as Bash).
        ExecutionMode::Sequential
    }

    async fn execute(
        &self,
        tool_call_id: &str,
        params: JsonValue,
        signal: CancellationToken,
        ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        let executor = match &self.executor {
            Ok(executor) => executor,
            Err(error) => {
                return Ok(AgentToolResult::error(format!(
                    "Codemode is unavailable: {error}"
                )));
            }
        };
        let Some(code) = params.get("code").and_then(JsonValue::as_str) else {
            return Err(ToolError::InvalidArguments(
                "the `code` parameter is required and must be a string (a JS async function body)"
                    .into(),
            ));
        };
        let owned_ctx = owned_context(ctx)?;
        for adapter in &self.adapters {
            adapter.install(tool_call_id, Arc::clone(&owned_ctx));
        }
        let snapshot: BTreeMap<String, String> =
            self.store.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let result = executor.execute(code, &snapshot, signal).await;

        // In-session continuity only: a successful execution's writes land in
        // the memory map the next execution starts from. Failed scripts never
        // produce writes (the bridge guarantees an empty write set).
        if result.error.is_none() {
            let mut store = self.store.lock().unwrap_or_else(|e| e.into_inner());
            for (key, json) in &result.store_writes.set {
                store.insert(key.clone(), json.clone());
            }
            for key in &result.store_writes.delete {
                store.remove(key);
            }
        }

        // The @options the script declared (defaults when unset); a source
        // error fails the execution before anything else, so `None` here is
        // unobservable in a successful detail row.
        let options = manox_codemode::source::parse_source(code)
            .ok()
            .map(|parsed| {
                serde_json::json!({
                    "maxOutputTokens": parsed.options.max_output_tokens,
                    "timeoutMs": parsed.options.timeout_ms,
                })
            });
        let mut full_output_path = None;
        let mut content: Vec<ContentBlock> = Vec::new();
        let header = match &result.error {
            None => format!(
                "Script completed in {:.1}s.",
                result.wall_time.as_secs_f64()
            ),
            Some(error) => format!(
                "Script failed in {:.1}s ({}).",
                result.wall_time.as_secs_f64(),
                error_kind_name(error.kind)
            ),
        };
        content.push(ContentBlock::Text {
            text: header,
            signature: None,
        });
        // The inline text is the crate's rendered output — body text items in
        // order plus the trailing `<console_output>` block, soft-truncated to
        // the script's budget. This block (not the raw `output` items) is what
        // bounds what the model sees; the untruncated text travels via
        // `full_output` when the budget fired.
        if !result.text.is_empty() {
            content.push(ContentBlock::Text {
                text: result.text.clone(),
                signature: None,
            });
        }
        // Images cannot join the text block: they stay independent blocks,
        // preserving their relative output order, after the text.
        for item in &result.output {
            if let manox_codemode::OutputItem::Image { data, mime_type } = item {
                content.push(ContentBlock::Image {
                    data: data.clone(),
                    mime_type: mime_type.clone(),
                });
            }
        }
        if let Some(value) = &result.value {
            content.push(ContentBlock::Text {
                text: format!("Return value: {value}"),
                signature: None,
            });
        }
        if let Some(error) = &result.error {
            let mut text = format!("Script error: {}", error.message);
            if let Some(stack) = &error.stack {
                text.push('\n');
                text.push_str(stack);
            }
            content.push(ContentBlock::Text {
                text,
                signature: None,
            });
        }
        // Soft-truncated output: the full text lands in a temp file the model
        // can read back with the file tools — the truncation must not destroy
        // the data, only the inline budget.
        if let Some(full_output) = &result.full_output {
            match write_full_output(tool_call_id, full_output) {
                Ok(path) => {
                    full_output_path = Some(path.display().to_string());
                    content.push(ContentBlock::Text {
                        text: format!(
                            "Output exceeded the inline budget and was truncated in place; the \
                             full text is saved at {}.",
                            path.display()
                        ),
                        signature: None,
                    });
                }
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        "could not persist the codemode full output; the truncated view stands"
                    );
                }
            }
        }

        let is_error = result.error.is_some();
        let store_writes = if is_error {
            None
        } else {
            Some(serde_json::to_value(&result.store_writes).unwrap_or(JsonValue::Null))
        };
        let details = serde_json::json!({
            "calls": result.calls,
            "storeWrites": store_writes,
            "options": options,
            "wallTimeMs": u64::try_from(result.wall_time.as_millis()).unwrap_or(u64::MAX),
            "droppedTools": executor.rendered_declarations().dropped,
            "fullOutputPath": full_output_path,
        });
        Ok(AgentToolResult {
            content,
            details: Some(details),
            is_error,
            usage: None,
            added_tool_names: None,
            terminate: false,
        })
    }
}

/// Rebuild an owned context from the incoming one. The bridge spawns its
/// nested calls, so the borrowed `&dyn ToolContext` cannot cross; a context
/// that owns neither handle cannot back nested executions and is refused
/// loudly (every production context owns both).
fn owned_context(ctx: &dyn ToolContext) -> Result<Arc<dyn ToolContext>, ToolError> {
    match (ctx.env_handle(), ctx.tool_state_handle()) {
        (Some(env), Some(state)) => Ok(Arc::new(LocalToolContext::new(
            env,
            ctx.cwd().to_path_buf(),
            state,
        ))),
        (None, Some(_)) => Err(ToolError::Other(
            "the tool context does not own its environment; codemode nested calls need an owned \
             context"
                .into(),
        )),
        (_, None) => Err(ToolError::Other(
            "the tool context does not own its tool state; codemode nested calls need an owned \
             context"
                .into(),
        )),
    }
}

/// Persist the untruncated output next to the session's other scratch: a
/// temp file named for the call, so a script's too-large answer stays
/// recoverable through the file tools.
fn write_full_output(tool_call_id: &str, full_output: &str) -> std::io::Result<std::path::PathBuf> {
    let safe: String = tool_call_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let path = std::env::temp_dir().join(format!("manox-codemode-{safe}.txt"));
    std::fs::write(&path, full_output)?;
    Ok(path)
}

/// The model-facing name of a failure kind (the same snake_case words the
/// details row serializes).
fn error_kind_name(kind: manox_codemode::CodemodeErrorKind) -> &'static str {
    match kind {
        manox_codemode::CodemodeErrorKind::Script => "script",
        manox_codemode::CodemodeErrorKind::Timeout => "timeout",
        manox_codemode::CodemodeErrorKind::Aborted => "aborted",
        manox_codemode::CodemodeErrorKind::Sandbox => "sandbox",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::TokioExecutionEnv;
    use crate::tool::ToolState;
    use std::sync::Mutex as StdMutex;

    fn ctx() -> LocalToolContext {
        LocalToolContext::new(
            Arc::new(TokioExecutionEnv::new(std::env::temp_dir())),
            std::env::temp_dir(),
            Arc::new(ToolState::new()),
        )
    }

    /// A sibling fake with a canned JSON answer and an optional delay. The
    /// shared trace records `start:<name>` / `end:<name>` intervals plus the
    /// nested `id:<call_id>` each execution observed.
    struct FakeSibling {
        name: String,
        sequential: bool,
        delay_ms: u64,
        response: Result<JsonValue, String>,
        trace: Arc<StdMutex<Vec<String>>>,
        schema: JsonValue,
    }

    #[async_trait::async_trait]
    impl AgentTool for FakeSibling {
        fn name(&self) -> &str {
            &self.name
        }
        fn description(&self) -> &str {
            "fake sibling"
        }
        fn parameters_schema(&self) -> JsonValue {
            self.schema.clone()
        }
        fn execution_mode(&self) -> ExecutionMode {
            if self.sequential {
                ExecutionMode::Sequential
            } else {
                ExecutionMode::Parallel
            }
        }
        async fn execute(
            &self,
            tool_call_id: &str,
            _params: JsonValue,
            signal: CancellationToken,
            _ctx: &dyn ToolContext,
        ) -> Result<AgentToolResult, ToolError> {
            {
                let mut trace = self.trace.lock().unwrap();
                trace.push(format!("id:{tool_call_id}"));
                trace.push(format!("start:{}", self.name));
            }
            if self.delay_ms > 0 {
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)) => {}
                    _ = signal.cancelled() => {}
                }
            }
            self.trace
                .lock()
                .unwrap()
                .push(format!("end:{}", self.name));
            match &self.response {
                Ok(value) => Ok(AgentToolResult {
                    content: vec![ContentBlock::Text {
                        text: value.to_string(),
                        signature: None,
                    }],
                    details: Some(serde_json::json!({"fake": true})),
                    is_error: false,
                    usage: None,
                    added_tool_names: None,
                    terminate: false,
                }),
                Err(text) => Err(ToolError::ExecutionFailed(text.clone())),
            }
        }
    }

    fn fake(
        name: &str,
        sequential: bool,
        delay_ms: u64,
        response: Result<JsonValue, String>,
        trace: &Arc<StdMutex<Vec<String>>>,
    ) -> Arc<dyn AgentTool> {
        fake_with_schema(
            name,
            sequential,
            delay_ms,
            response,
            trace,
            serde_json::json!({"type": "object"}),
        )
    }

    fn fake_with_schema(
        name: &str,
        sequential: bool,
        delay_ms: u64,
        response: Result<JsonValue, String>,
        trace: &Arc<StdMutex<Vec<String>>>,
        schema: JsonValue,
    ) -> Arc<dyn AgentTool> {
        Arc::new(FakeSibling {
            name: name.to_string(),
            sequential,
            delay_ms,
            response,
            trace: Arc::clone(trace),
            schema,
        })
    }

    /// The body texts of one execution's non-console output items.
    fn body_texts(result: &manox_codemode::CodemodeResult) -> Vec<String> {
        result
            .output
            .iter()
            .filter_map(|item| match item {
                manox_codemode::OutputItem::Text {
                    text,
                    console: false,
                } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    async fn run_script(
        tool: &CodemodeTool,
        call_id: &str,
        code: &str,
    ) -> manox_codemode::CodemodeResult {
        let executor = tool.executor.as_ref().expect("executor builds");
        let owned = owned_context(&ctx()).unwrap();
        for adapter in &tool.adapters {
            adapter.install(call_id, Arc::clone(&owned));
        }
        let snapshot = tool.store.lock().unwrap().clone();
        executor
            .execute(code, &snapshot, CancellationToken::new())
            .await
    }

    #[tokio::test]
    async fn nested_roundtrip_and_error_text_penetrate() {
        let trace = Arc::new(StdMutex::new(Vec::new()));
        let echo = fake(
            "Echo",
            false,
            0,
            Ok(serde_json::json!({"got": "it"})),
            &trace,
        );
        let failing = fake(
            "Boom",
            false,
            0,
            Err("the file does not exist".to_string()),
            &trace,
        );
        let tool = CodemodeTool::new(vec![echo, failing], BTreeMap::new());

        // The happy path: the script sees the adapted result object —
        // output text, error flag, details — not the raw block list.
        let result = run_script(
            &tool,
            "call-1",
            "const r = await tools.Echo({x: 1}); text(JSON.stringify(r));",
        )
        .await;
        assert!(result.error.is_none(), "{:?}", result.error);
        let echoed: JsonValue =
            serde_json::from_str(&body_texts(&result)[0]).expect("the echoed result JSON");
        let expected_output = serde_json::json!({"got": "it"}).to_string();
        assert_eq!(echoed["output"], serde_json::json!(expected_output));
        assert_eq!(echoed["isError"], serde_json::json!(false));
        assert_eq!(echoed["details"], serde_json::json!({"fake": true}));

        // An Err from the sibling penetrates as the rejection text.
        let result = run_script(
            &tool,
            "call-2",
            "try { await tools.Boom({}); } catch (e) { text(String(e.message ?? e)); }",
        )
        .await;
        assert!(result.error.is_none());
        assert_eq!(result.calls.len(), 1);
        assert_eq!(result.calls[0].name, "Boom");
        assert_eq!(result.calls[0].status, manox_codemode::CallStatus::Error);
        assert_eq!(
            result.calls[0].error.as_deref(),
            Some("execution failed: the file does not exist")
        );
        assert!(body_texts(&result)[0].contains("the file does not exist"));

        // The nested id carries the parent call id as its prefix (the VM's
        // call counter is 1-based).
        let trace = trace.lock().unwrap();
        assert!(
            trace.iter().any(|line| line == "id:call-2:1"),
            "nested id must be <parent>:<script id>: {trace:?}"
        );
    }

    /// The `details` row the journal replays: stable camelCase keys, writes
    /// only on success, the call records, and the truncation path when the
    /// soft budget fired.
    #[tokio::test]
    async fn details_shape_is_stable_and_serializable() {
        let trace = Arc::new(StdMutex::new(Vec::new()));
        let echo = fake("Echo", false, 0, Ok(serde_json::json!(1)), &trace);
        let tool = CodemodeTool::new(vec![echo], BTreeMap::new());

        let result = tool
            .execute(
                "call-1",
                serde_json::json!({"code": "store('k', 7); await tools.Echo({});"}),
                CancellationToken::new(),
                &ctx(),
            )
            .await
            .unwrap();
        assert!(!result.is_error);
        let details = result.details.expect("details row");
        assert_eq!(
            details["storeWrites"]["set"],
            serde_json::json!([["k", "7"]])
        );
        assert_eq!(details["storeWrites"]["delete"], serde_json::json!([]));
        assert_eq!(details["droppedTools"], serde_json::json!([]));
        assert_eq!(details["fullOutputPath"], JsonValue::Null);
        assert_eq!(
            details["options"]["maxOutputTokens"],
            serde_json::json!(10000)
        );
        assert_eq!(details["options"]["timeoutMs"], JsonValue::Null);
        assert_eq!(details["calls"].as_array().map(Vec::len), Some(1));
        assert!(details["wallTimeMs"].is_u64());

        // A failed script omits the writes entirely.
        let result = tool
            .execute(
                "call-2",
                serde_json::json!({"code": "throw new Error('no');"}),
                CancellationToken::new(),
                &ctx(),
            )
            .await
            .unwrap();
        assert!(result.is_error);
        let details = result.details.expect("details row");
        assert_eq!(details["storeWrites"], JsonValue::Null);
        assert!(
            serde_json::to_string(&details).is_ok(),
            "details must stay serializable"
        );
    }

    /// D18: siblings declaring `Sequential` never overlap inside a script,
    /// while parallel siblings do. The sequential pair proves the ordering
    /// with interval traces (end of one strictly before the start of the
    /// other); the parallel pair proves concurrency with a two-party barrier.
    #[tokio::test]
    async fn sequential_siblings_serialize_and_parallel_siblings_overlap() {
        let trace = Arc::new(StdMutex::new(Vec::new()));
        let seq_a = fake("SeqA", true, 60, Ok(serde_json::json!(1)), &trace);
        let seq_b = fake("SeqB", true, 60, Ok(serde_json::json!(2)), &trace);
        let tool = CodemodeTool::new(vec![seq_a, seq_b], BTreeMap::new());
        let result = run_script(
            &tool,
            "call-1",
            "await Promise.all([tools.SeqA({}), tools.SeqB({})]);",
        )
        .await;
        assert!(result.error.is_none(), "{:?}", result.error);
        // The interval trace is read in its own scope: the guard must drop
        // before the second execution's await below.
        {
            let t = trace.lock().unwrap();
            let idx = |tag: &str| {
                t.iter()
                    .position(|e| e == tag)
                    .unwrap_or_else(|| panic!("{tag} missing: {t:?}"))
            };
            // No interval overlap: one sibling's end lands strictly before
            // the other's start. Two sequential siblings can never be
            // concurrent.
            let a_before_b = idx("end:SeqA") < idx("start:SeqB");
            let b_before_a = idx("end:SeqB") < idx("start:SeqA");
            assert!(
                a_before_b ^ b_before_a,
                "sequential siblings must not overlap: {t:?}"
            );
        }

        // Contrast: two parallel siblings run concurrently — a two-party
        // barrier inside both is only passable when their executions overlap.
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        struct BarrierSibling {
            name: &'static str,
            barrier: Arc<tokio::sync::Barrier>,
        }
        #[async_trait::async_trait]
        impl AgentTool for BarrierSibling {
            fn name(&self) -> &str {
                self.name
            }
            fn description(&self) -> &str {
                "barrier sibling"
            }
            fn parameters_schema(&self) -> JsonValue {
                serde_json::json!({"type": "object"})
            }
            async fn execute(
                &self,
                _: &str,
                _: JsonValue,
                _: CancellationToken,
                _: &dyn ToolContext,
            ) -> Result<AgentToolResult, ToolError> {
                self.barrier.wait().await;
                Ok(AgentToolResult::text("passed"))
            }
        }
        let tool = CodemodeTool::new(
            vec![
                Arc::new(BarrierSibling {
                    name: "ParA",
                    barrier: Arc::clone(&barrier),
                }),
                Arc::new(BarrierSibling {
                    name: "ParB",
                    barrier: Arc::clone(&barrier),
                }),
            ],
            BTreeMap::new(),
        );
        let result = run_script(
            &tool,
            "call-2",
            "const rs = await Promise.all([tools.ParA({}), tools.ParB({})]); \
             text(String(rs.length));",
        )
        .await;
        assert!(
            result.error.is_none(),
            "parallel siblings must overlap (the barrier proves it): {:?}",
            result.error
        );
    }

    /// The script-tool surface excludes Codemode itself by construction, and
    /// the description carries the prose and the declarations (with the
    /// dropped-tools hint absent while everything fits).
    #[test]
    fn description_carries_prose_and_declarations_without_self() {
        let trace = Arc::new(StdMutex::new(Vec::new()));
        let echo = fake("Echo", false, 0, Ok(serde_json::json!(1)), &trace);
        let tool = CodemodeTool::new(vec![echo], BTreeMap::new());
        let names = tool.script_tool_names();
        assert_eq!(names, vec!["Echo".to_string()]);
        assert!(!names.contains(&"Codemode".to_string()));
        let description = tool.description();
        assert!(description.contains("async function body"), "prose present");
        assert!(description.contains("Echo("), "declaration present");
        assert!(!description.contains("too large to declare"));
    }

    /// The description's third part: a sibling whose sample blows the
    /// declaration budget is dropped to a named hint (still callable,
    /// discoverable via `ALL_TOOLS` / `describeTool`), and its declaration is
    /// absent from the listing while the smaller sibling's stays.
    #[test]
    fn an_oversized_sibling_drops_to_the_named_discovery_hint() {
        let trace = Arc::new(StdMutex::new(Vec::new()));
        let echo = fake("Echo", false, 0, Ok(serde_json::json!(1)), &trace);
        // 13k chars of property description: the rendered type stays under
        // the schema renderer's own 16k degradation ceiling (which would
        // collapse it to `unknown` and hide the size) while the description +
        // declaration sample blows the 3000-token budget in both the tagged
        // and the untagged render.
        let oversized = fake_with_schema(
            "Huge",
            false,
            0,
            Ok(serde_json::json!(1)),
            &trace,
            serde_json::json!({
                "type": "object",
                "properties": {
                    "data": {"type": "string", "description": "x".repeat(13_000)}
                }
            }),
        );
        let tool = CodemodeTool::new(vec![echo, oversized], BTreeMap::new());
        // A dropped sibling stays script-callable — only its inline
        // declaration is gone.
        assert_eq!(
            tool.script_tool_names(),
            vec!["Echo".to_string(), "Huge".to_string()]
        );
        let description = tool.description();
        assert!(
            !description.contains("Huge("),
            "no declaration: {description}"
        );
        assert!(
            description.contains("Echo("),
            "the fitting sibling declares"
        );
        assert!(
            description.contains("still callable"),
            "the hint: {description}"
        );
        assert!(description.contains("Huge"), "named in the hint");
    }

    /// A duplicate sibling name degrades to an unavailable tool with the
    /// reason in the description and an error result on execute — the
    /// assembly never dies for it.
    #[tokio::test]
    async fn duplicate_sibling_names_fail_the_tool_not_the_assembly() {
        let trace = Arc::new(StdMutex::new(Vec::new()));
        let a = fake("Dup", false, 0, Ok(serde_json::json!(1)), &trace);
        let b = fake("Dup", false, 0, Ok(serde_json::json!(2)), &trace);
        let tool = CodemodeTool::new(vec![a, b], BTreeMap::new());
        assert!(tool.executor.is_err());
        assert!(tool.script_tool_names().is_empty());
        assert!(tool.description().contains("UNAVAILABLE"));
        let result = tool
            .execute(
                "call-1",
                serde_json::json!({"code": "text('x');"}),
                CancellationToken::new(),
                &ctx(),
            )
            .await
            .unwrap();
        assert!(result.is_error);
    }

    /// The in-memory store folds each successful execution's writes and
    /// ignores a failed one; a delete removes the key for the next script.
    /// The fold lives on the `AgentTool::execute` path, so this test drives
    /// the tool face and reads continuity through what the NEXT script's
    /// `load` sees.
    #[tokio::test]
    async fn store_updates_follow_successful_executions() {
        let trace = Arc::new(StdMutex::new(Vec::new()));
        let echo = fake("Echo", false, 0, Ok(serde_json::json!(1)), &trace);
        let tool = CodemodeTool::new(vec![echo], BTreeMap::new());
        async fn run(tool: &CodemodeTool, call_id: &str, code: &str) -> AgentToolResult {
            tool.execute(
                call_id,
                serde_json::json!({ "code": code }),
                CancellationToken::new(),
                &ctx(),
            )
            .await
            .unwrap()
        }
        let body = |result: &AgentToolResult| -> String {
            let ContentBlock::Text { text, .. } = &result.content[1] else {
                panic!("expected the body text block");
            };
            text.clone()
        };

        // Load reads back what a previous execution stored.
        let result = run(&tool, "c1", "store('a', 1); store('b', 2);").await;
        assert!(!result.is_error);
        let result = run(&tool, "c2", "text(`${load('a')},${load('b')}`);").await;
        assert_eq!(body(&result), "1,2");

        // A failed script writes nothing: the store('c') never lands.
        let result = run(&tool, "c3", "store('c', 3); throw new Error('no');").await;
        assert!(result.is_error);
        let result = run(&tool, "c4", "text(String(load('c')));").await;
        assert_eq!(body(&result), "undefined");

        // Delete + overwrite fold into what the next script sees.
        let result = run(&tool, "c5", "store('a', undefined); store('b', 9);").await;
        assert!(!result.is_error);
        let result = run(&tool, "c6", "text(`${load('a')},${load('b')}`);").await;
        assert_eq!(body(&result), "undefined,9");
    }

    /// The constructor's initial store (the journal replay's fold) seeds the
    /// first execution: `load` sees the planted keys before any write.
    #[tokio::test]
    async fn initial_store_seeds_the_first_execution() {
        let trace = Arc::new(StdMutex::new(Vec::new()));
        let echo = fake("Echo", false, 0, Ok(serde_json::json!(1)), &trace);
        let tool = CodemodeTool::new(
            vec![echo],
            BTreeMap::from([("seed".to_string(), "\"planted\"".to_string())]),
        );
        let result = run_script(&tool, "c1", "text(String(load('seed')));").await;
        assert!(result.error.is_none(), "{:?}", result.error);
        // `load` returns the parsed JSON value, so the stored "planted"
        // string renders without its JSON quotes.
        assert_eq!(body_texts(&result), vec!["planted".to_string()]);
    }

    /// Soft-truncated output lands in a temp file named after the call and
    /// the path is reported both in the details and in the result text. The
    /// inline text block is the crate's truncated rendering (omission marker
    /// present, near the budget), not the raw 80k-char output — the default
    /// budget actually bounds what reaches the model.
    #[tokio::test]
    async fn truncated_full_output_reaches_a_temp_file() {
        let trace = Arc::new(StdMutex::new(Vec::new()));
        let echo = fake("Echo", false, 0, Ok(serde_json::json!(1)), &trace);
        let tool = CodemodeTool::new(vec![echo], BTreeMap::new());
        let result = tool
            .execute(
                "call/big:1",
                serde_json::json!({"code": "text('x'.repeat(80000));"}),
                CancellationToken::new(),
                &ctx(),
            )
            .await
            .unwrap();
        assert!(!result.is_error);
        let details = result.details.expect("details row");
        let path =
            std::path::PathBuf::from(details["fullOutputPath"].as_str().expect("path reported"));
        assert_eq!(
            path,
            std::env::temp_dir().join("manox-codemode-call_big_1.txt")
        );
        let persisted = std::fs::read_to_string(&path).expect("full output persisted");
        assert_eq!(persisted.chars().count(), 80_000);
        // The inline body is truncated in place: marker inside, size near the
        // 10k-token (40k chars) budget — never the full 80k chars.
        let ContentBlock::Text { text, .. } = &result.content[1] else {
            panic!("the body text block follows the header");
        };
        assert!(text.contains("characters omitted"), "{text}");
        assert!(
            text.chars().count() < 50_000,
            "inline text must be truncated, got {}",
            text.chars().count()
        );
        let ContentBlock::Text { text, .. } = &result.content[result.content.len() - 1] else {
            panic!("the truncation note is the last text block");
        };
        assert!(text.contains(&path.display().to_string()), "{text}");
        let _ = std::fs::remove_file(&path);
    }

    /// The session-facing content folds the crate's rendered text: body items
    /// in order, one trailing `<console_output>` block, images as independent
    /// blocks after the text — never per-item console blocks inline.
    #[tokio::test]
    async fn inline_content_is_rendered_text_plus_image_blocks() {
        let trace = Arc::new(StdMutex::new(Vec::new()));
        let echo = fake("Echo", false, 0, Ok(serde_json::json!(1)), &trace);
        let tool = CodemodeTool::new(vec![echo], BTreeMap::new());
        let code = "text('a'); console.log('c1'); text('b'); \
                    image('data:image/png;base64,iVBORw0KGgo=');";
        let result = tool
            .execute(
                "call-1",
                serde_json::json!({"code": code}),
                CancellationToken::new(),
                &ctx(),
            )
            .await
            .unwrap();
        assert!(!result.is_error);
        // header, single rendered text block, one image block.
        assert_eq!(result.content.len(), 3, "{:?}", result.content);
        let ContentBlock::Text { text, .. } = &result.content[1] else {
            panic!("second block is the rendered text");
        };
        assert_eq!(
            text, "a\nb\n<console_output>\nc1\n</console_output>",
            "console folds into the trailing block"
        );
        assert!(matches!(&result.content[2], ContentBlock::Image { .. }));
    }
}
