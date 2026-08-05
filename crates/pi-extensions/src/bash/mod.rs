// Bash tool enhancements — the product-level bash tool built on the core
// `BashOperations` / `BackgroundTaskRegistry` seams.
//
// The wrapper keeps the core bash tool untouched: it widens the parameter
// schema (cwd, run_in_background, head/tail line filters) and dispatches
// either to the injected execution backend or to the background registry.

pub mod background;
pub mod orchestration;
pub mod persistent;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use pi::BackgroundTaskRegistry;
use pi::env::CommandResult;

use orchestration::BackgroundManager;
use pi::tool::{AgentTool, AgentToolResult, ToolContext, ToolError};
use pi::tools::bash::{BashExecRequest, BashOperations};
use pi::tools::truncate::{self, TruncateConfig};
use serde_json::Value as JsonValue;
use tokio_util::sync::CancellationToken;

/// Default max bytes for output.
const DEFAULT_MAX_BYTES: usize = 128 * 1024;
/// Default max lines for output.
const DEFAULT_MAX_LINES: usize = 2000;
/// Wall-clock limit before a hung command is killed.
const DEFAULT_TIMEOUT_MS: u64 = 120_000;

/// The bash tool with the manox product surface: optional `cwd`, background
/// execution via the registry, and head/tail line filters on top of the core
/// execution semantics.
pub struct BashTool {
    operations: Arc<dyn BashOperations>,
    registry: Arc<dyn BackgroundTaskRegistry>,
    command_prefix: Option<String>,
    /// Optional orchestrator: when bound, background tasks are registered,
    /// watched, and their completions steered into the agent session.
    manager: Option<Arc<BackgroundManager>>,
}

impl BashTool {
    pub fn new(
        operations: Arc<dyn BashOperations>,
        registry: Arc<dyn BackgroundTaskRegistry>,
    ) -> Self {
        BashTool {
            operations,
            registry,
            command_prefix: None,
            manager: None,
        }
    }

    /// Prepend a command to every invocation (shell setup commands).
    pub fn with_command_prefix(mut self, prefix: Option<String>) -> Self {
        self.command_prefix = prefix;
        self
    }

    /// Bind an orchestrator so background tasks participate in the agent
    /// session's lifecycle.
    pub fn with_manager(mut self, manager: Arc<BackgroundManager>) -> Self {
        self.manager = Some(manager);
        self
    }
}

#[async_trait::async_trait]
impl AgentTool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Execute a shell command. State (cwd, exported vars, functions) persists across calls. \
         Optionally run in the background with `run_in_background: true` — the command starts in a \
         fresh shell (no persistent state) at the session cwd — and collect output via `bash_output`; \
         stop it with `task_stop`. Use `head_lines`/`tail_lines` to keep a selection of the output \
         instead of piping through `head`/`tail`."
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn requires_approval(&self, _params: &JsonValue) -> bool {
        true
    }

    /// The persistent shell is stateful: a batch of parallel bash calls
    /// would interleave their mutations.
    fn execution_mode(&self) -> pi::tool::ExecutionMode {
        pi::tool::ExecutionMode::Sequential
    }

    fn parameters_schema(&self) -> JsonValue {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The command to execute"
                },
                "timeout": {
                    "type": "integer",
                    "description": "Timeout in milliseconds (default: 120000)"
                },
                "cwd": {
                    "type": "string",
                    "description": "Working directory; defaults to the session cwd. The shell's cwd persists across calls"
                },
                "run_in_background": {
                    "type": "boolean",
                    "description": "Start the command in the background and return a task id immediately; poll with bash_output, stop with task_stop"
                },
                "head_lines": {
                    "type": "integer",
                    "description": "Keep only the first N lines of output"
                },
                "tail_lines": {
                    "type": "integer",
                    "description": "Keep only the last N lines of output"
                }
            },
            "required": ["command"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: JsonValue,
        signal: CancellationToken,
        ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        self.run(params, signal, ctx, None).await
    }

    async fn execute_with_progress(
        &self,
        _tool_call_id: &str,
        params: JsonValue,
        signal: CancellationToken,
        ctx: &dyn ToolContext,
        progress: &dyn pi::tool::ToolProgress,
    ) -> Result<AgentToolResult, ToolError> {
        let on_data = |data: &[u8]| {
            progress.emit(serde_json::json!({
                "output": String::from_utf8_lossy(data),
            }));
        };
        self.run(params, signal, ctx, Some(&on_data)).await
    }
}

impl BashTool {
    async fn run(
        &self,
        params: JsonValue,
        signal: CancellationToken,
        ctx: &dyn ToolContext,
        on_data: Option<pi::tools::bash::BashDataCallback<'_>>,
    ) -> Result<AgentToolResult, ToolError> {
        let command = params["command"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("command is required".into()))?;
        let timeout_ms = params["timeout"].as_u64().unwrap_or(DEFAULT_TIMEOUT_MS);
        // An explicit cwd override re-pins the shell; absent, the shell's
        // current directory (kept across `cd`) is used.
        let cwd_override = resolve_cwd(params.get("cwd"), ctx.cwd());
        let run_cwd = cwd_override.as_deref().unwrap_or_else(|| ctx.cwd());
        let run_in_background = params["run_in_background"].as_bool().unwrap_or(false);
        let head_lines = params["head_lines"].as_u64().map(|v| v as usize);
        let tail_lines = params["tail_lines"].as_u64().map(|v| v as usize);

        let command = if let Some(ref prefix) = self.command_prefix {
            format!("{prefix}\n{command}")
        } else {
            command.to_string()
        };

        if run_in_background {
            let id = match &self.manager {
                Some(manager) => manager
                    .spawn(&command, run_cwd)
                    .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?,
                None => self
                    .registry
                    .spawn(&command, run_cwd)
                    .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?,
            };
            return Ok(AgentToolResult::text(format!(
                "Started in background as `{id}`. Poll with `bash_output` (shell_id), stop with `task_stop`."
            )));
        }

        let result = self
            .operations
            .exec(BashExecRequest {
                command: &command,
                cwd: cwd_override.as_deref(),
                timeout: Some(Duration::from_millis(timeout_ms)),
                signal,
                on_data,
            })
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("{e}")))?;

        Ok(AgentToolResult::text(assemble_output(
            result, head_lines, tail_lines,
        )))
    }
}

/// Resolve a possibly-relative cwd override against the session cwd.
fn resolve_cwd(cwd: Option<&JsonValue>, base: &Path) -> Option<PathBuf> {
    cwd.and_then(|v| v.as_str()).map(|p| {
        let p = Path::new(p);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            base.join(p)
        }
    })
}

/// Assemble the model-facing output: stderr merged after stdout, exit code
/// annotated, head/tail filtered, then truncated.
fn assemble_output(result: CommandResult, head: Option<usize>, tail: Option<usize>) -> String {
    let mut output = result.stdout;

    if !result.stderr.is_empty() {
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str("[stderr]\n");
        output.push_str(&result.stderr);
    }

    if result.exit_code != 0 {
        output.push_str(&format!("\n\n[exit code: {}]", result.exit_code));
    }

    let output = select_lines(&output, head, tail);

    let config = TruncateConfig {
        max_bytes: DEFAULT_MAX_BYTES,
        max_lines: DEFAULT_MAX_LINES,
    };
    let truncated = truncate::truncate(&output, &config);
    let mut final_output = truncated.content;
    if truncated.was_truncated {
        final_output.push_str(&format!(
            "\n\n[output truncated: {} lines, {} bytes]",
            truncated.original_lines, truncated.original_bytes
        ));
    }
    final_output
}

/// Keep the first `head` and/or last `tail` lines of the output.
fn select_lines(text: &str, head: Option<usize>, tail: Option<usize>) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let len = lines.len();
    match (head, tail) {
        (Some(h), Some(t)) if h + t < len => {
            let mut kept: Vec<&str> = lines[..h].to_vec();
            kept.push("...");
            kept.extend_from_slice(&lines[len - t..]);
            kept.join("\n")
        }
        (Some(h), _) => lines[..h.min(len)].join("\n"),
        (_, Some(t)) => lines[len - t.min(len)..].join("\n"),
        _ => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi::tools::bash::BashOperations;
    use std::sync::Mutex;

    struct EchoOps;
    #[async_trait::async_trait]
    impl BashOperations for EchoOps {
        async fn exec(
            &self,
            request: BashExecRequest<'_>,
        ) -> Result<CommandResult, pi::env::ExecutionError> {
            let output = format!(
                "cmd={};cwd={}",
                request.command,
                request
                    .cwd
                    .map(|c| c.display().to_string())
                    .unwrap_or_default()
            );
            if let Some(on_data) = request.on_data {
                on_data(output.as_bytes());
            }
            Ok(CommandResult {
                stdout: output,
                stderr: String::new(),
                exit_code: 0,
            })
        }
    }

    struct NoopRegistry;
    #[async_trait::async_trait]
    impl BackgroundTaskRegistry for NoopRegistry {
        fn spawn(&self, _command: &str, _cwd: &Path) -> Result<pi::TaskId, pi::TaskError> {
            Ok(pi::TaskId("bg_test".into()))
        }
        async fn poll(&self, _id: &pi::TaskId) -> Result<pi::PollResult, pi::TaskError> {
            Ok(pi::PollResult {
                new_output: String::new(),
                is_running: false,
                exit_code: Some(Some(0)),
                total_bytes: 0,
            })
        }
        async fn kill(&self, _id: &pi::TaskId) -> Result<(), pi::TaskError> {
            Ok(())
        }
    }

    type RecordedCall = (String, Option<PathBuf>, Option<Duration>);

    struct RecordingOps {
        calls: Mutex<Vec<RecordedCall>>,
    }
    #[async_trait::async_trait]
    impl BashOperations for RecordingOps {
        async fn exec(
            &self,
            request: BashExecRequest<'_>,
        ) -> Result<CommandResult, pi::env::ExecutionError> {
            self.calls.lock().unwrap().push((
                request.command.to_string(),
                request.cwd.map(|c| c.to_path_buf()),
                request.timeout,
            ));
            Ok(CommandResult {
                stdout: "out".into(),
                stderr: "err".into(),
                exit_code: 2,
            })
        }
    }

    /// Minimal `ExecutionEnv` standing in for the harness environment.
    struct MockEnv {
        cwd: PathBuf,
    }

    #[async_trait::async_trait]
    impl pi::env::ExecutionEnv for MockEnv {
        fn cwd(&self) -> &Path {
            &self.cwd
        }
        fn join_path(&self, parts: &[&str]) -> PathBuf {
            parts.iter().collect()
        }
        async fn absolute_path(&self, path: &Path) -> Result<PathBuf, pi::env::FileError> {
            Ok(path.to_path_buf())
        }
        async fn read_file(
            &self,
            _path: &Path,
            _offset: Option<usize>,
            _limit: Option<usize>,
        ) -> Result<String, pi::env::FileError> {
            Ok(String::new())
        }
        async fn write_file(&self, _path: &Path, _content: &str) -> Result<(), pi::env::FileError> {
            Ok(())
        }
        async fn exists(&self, _path: &Path) -> Result<bool, pi::env::FileError> {
            Ok(true)
        }
        async fn file_info(&self, _path: &Path) -> Result<pi::env::FileInfo, pi::env::FileError> {
            Ok(pi::env::FileInfo {
                path: _path.to_path_buf(),
                is_dir: false,
                size: 0,
            })
        }
        async fn list_dir(
            &self,
            _path: &Path,
        ) -> Result<Vec<pi::env::FileInfo>, pi::env::FileError> {
            Ok(vec![])
        }
        async fn create_dir(&self, _path: &Path) -> Result<(), pi::env::FileError> {
            Ok(())
        }
        async fn remove(&self, _path: &Path) -> Result<(), pi::env::FileError> {
            Ok(())
        }
        async fn exec(
            &self,
            _command: &str,
            _timeout: Duration,
            _signal: CancellationToken,
        ) -> Result<CommandResult, pi::env::ExecutionError> {
            Ok(CommandResult {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
            })
        }
    }

    fn ctx(cwd: &str) -> pi::tool::LocalToolContext {
        pi::tool::LocalToolContext::new(
            Arc::new(MockEnv {
                cwd: PathBuf::from(cwd),
            }),
            PathBuf::from(cwd),
            Arc::new(pi::tool::ToolState::new()),
        )
    }

    #[tokio::test]
    async fn dispatches_cwd_and_assembles_output() {
        let ops = Arc::new(RecordingOps {
            calls: Mutex::new(Vec::new()),
        });
        let tool = BashTool::new(ops.clone(), Arc::new(NoopRegistry));
        let result = tool
            .execute(
                "c1",
                serde_json::json!({"command": "echo hi", "cwd": "/work"}),
                CancellationToken::new(),
                &ctx("/base"),
            )
            .await
            .unwrap();
        let calls = ops.calls.lock().unwrap();
        assert_eq!(
            calls[0].1,
            Some(PathBuf::from("/work")),
            "cwd override reaches the backend"
        );
        assert!(!result.is_error);
    }

    #[tokio::test]
    async fn run_in_background_returns_a_task_id() {
        let tool = BashTool::new(Arc::new(EchoOps), Arc::new(NoopRegistry));
        let result = tool
            .execute(
                "c1",
                serde_json::json!({"command": "sleep 5", "run_in_background": true}),
                CancellationToken::new(),
                &ctx("/base"),
            )
            .await
            .unwrap();
        let text = match &result.content[0] {
            pi::types::ContentBlock::Text { text, .. } => text.clone(),
            _ => panic!("expected text"),
        };
        assert!(
            text.contains("bg_test"),
            "returns the background id: {text}"
        );
    }

    #[test]
    fn select_lines_keeps_head_and_tail() {
        let text = "a\nb\nc\nd\ne";
        assert_eq!(select_lines(text, Some(2), Some(2)), "a\nb\n...\nd\ne");
        assert_eq!(select_lines(text, Some(2), None), "a\nb");
        assert_eq!(select_lines(text, None, Some(2)), "d\ne");
        assert_eq!(select_lines(text, None, None), text);
    }
}
