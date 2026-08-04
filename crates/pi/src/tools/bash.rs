// Bash tool — shell command execution with output truncation.
//
// The tool assembles output identically regardless of the execution backend:
// the default backend is `ExecutionEnv::exec`, and extensions inject a custom
// `BashOperations` implementation (persistent shell, sandbox wrapper) without
// touching assembly or truncation.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value as JsonValue;
use tokio_util::sync::CancellationToken;

use crate::env::{CommandResult, ExecutionError};
use crate::tool::{AgentTool, AgentToolResult, ToolContext, ToolError};
use crate::tools::truncate::{self, TruncateConfig};

/// Streaming output callback for a bash run.
pub type BashDataCallback<'a> = &'a (dyn Fn(&[u8]) + Send + Sync);

/// A single shell command execution request.
///
/// `cwd` is explicit so injected backends are not coupled to the harness's
/// environment; `on_data` streams incremental output when the caller wants
/// real-time progress.
pub struct BashExecRequest<'a> {
    pub command: &'a str,
    /// The working directory the command runs in. `None` keeps the backend's
    /// current working directory (a persistent shell retains `cd`).
    pub cwd: Option<&'a Path>,
    pub timeout: Option<Duration>,
    pub signal: CancellationToken,
    pub on_data: Option<BashDataCallback<'a>>,
}

/// Pluggable execution backend for the `bash` tool.
///
/// Mirrors the upstream `BashOperations` seam: the tool only needs the final
/// aggregated result, while callers may also observe streaming chunks. The
/// default backend is `ExecutionEnv::exec`; extensions provide their own
/// implementation (persistent shell, sandbox) and inject it via
/// [`BashTool::with_operations`].
#[async_trait::async_trait]
pub trait BashOperations: Send + Sync {
    /// Execute a command, returning the aggregated output.
    async fn exec(&self, request: BashExecRequest<'_>) -> Result<CommandResult, ExecutionError>;
}

pub struct BashTool {
    command_prefix: Option<String>,
    operations: Option<Arc<dyn BashOperations>>,
}

impl BashTool {
    /// Create a bash tool backed by the harness `ExecutionEnv`.
    pub fn new(command_prefix: Option<String>) -> Self {
        BashTool {
            command_prefix,
            operations: None,
        }
    }

    /// Create a bash tool backed by an injected execution backend.
    pub fn with_operations(
        command_prefix: Option<String>,
        operations: Arc<dyn BashOperations>,
    ) -> Self {
        BashTool {
            command_prefix,
            operations: Some(operations),
        }
    }
}

impl BashTool {
    /// Default max bytes for output.
    const DEFAULT_MAX_BYTES: usize = 128 * 1024;
    /// Default max lines for output.
    const DEFAULT_MAX_LINES: usize = 2000;
}

#[async_trait::async_trait]
impl AgentTool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Execute a shell command"
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn requires_approval(&self, _params: &JsonValue) -> bool {
        true
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
        progress: &dyn crate::tool::ToolProgress,
    ) -> Result<AgentToolResult, ToolError> {
        // Stream the backend's incremental output through the existing
        // progress pipeline (ToolExecutionUpdate events) without adding a
        // new event surface.
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
        on_data: Option<BashDataCallback<'_>>,
    ) -> Result<AgentToolResult, ToolError> {
        let command = params["command"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("command is required".into()))?;
        let timeout_ms = params["timeout"].as_u64().unwrap_or(120_000);

        let command = if let Some(ref prefix) = self.command_prefix {
            format!("{prefix} {command}")
        } else {
            command.to_string()
        };

        let timeout = Duration::from_millis(timeout_ms);
        let result = match &self.operations {
            None => ctx
                .env()
                .exec(&command, timeout, signal)
                .await
                .map_err(|e| ToolError::ExecutionFailed(format!("{e}")))?,
            Some(ops) => ops
                .exec(BashExecRequest {
                    command: &command,
                    cwd: Some(ctx.cwd()),
                    timeout: Some(timeout),
                    signal,
                    on_data,
                })
                .await
                .map_err(|e| ToolError::ExecutionFailed(format!("{e}")))?,
        };

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

        // Truncate output to avoid overwhelming the context window.
        let config = TruncateConfig {
            max_bytes: Self::DEFAULT_MAX_BYTES,
            max_lines: Self::DEFAULT_MAX_LINES,
        };
        let truncated = truncate::truncate(&output, &config);

        let mut final_output = truncated.content;
        if truncated.was_truncated {
            final_output.push_str(&format!(
                "\n\n[output truncated: {} lines, {} bytes]",
                truncated.original_lines, truncated.original_bytes
            ));
        }

        Ok(AgentToolResult::text(final_output))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::ExecutionEnv;
    use crate::tool::{LocalToolContext, ToolState};
    use std::path::{Path, PathBuf};

    struct MockEnv {
        result: CommandResult,
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
            Ok(String::new())
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
                size: 0,
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
        ) -> Result<CommandResult, crate::env::ExecutionError> {
            Ok(self.result.clone())
        }
    }

    fn ctx_with_env(env: Arc<dyn ExecutionEnv>) -> LocalToolContext {
        LocalToolContext::new(env, PathBuf::from("/mock"), Arc::new(ToolState::new()))
    }

    #[tokio::test]
    async fn default_backend_is_execution_env() {
        let env = Arc::new(MockEnv {
            result: CommandResult {
                stdout: "out".into(),
                stderr: "err".into(),
                exit_code: 1,
            },
        });
        let tool = BashTool::new(None);
        let result = tool
            .execute(
                "c1",
                serde_json::json!({"command": "echo hi"}),
                CancellationToken::new(),
                &ctx_with_env(env),
            )
            .await
            .unwrap();
        let text = match &result.content[0] {
            crate::types::ContentBlock::Text { text, .. } => text.clone(),
            _ => panic!("expected text block"),
        };
        assert!(text.contains("out"));
        assert!(text.contains("[stderr]\nerr"));
        assert!(text.contains("[exit code: 1]"));
    }

    /// A backend recording the request it received and returning a canned
    /// result, standing in for a persistent-shell or sandbox implementation.
    struct RecordingOps {
        calls: std::sync::Mutex<Vec<(String, PathBuf, Option<Duration>)>>,
        on_data_calls: std::sync::Mutex<usize>,
        result: CommandResult,
    }

    #[async_trait::async_trait]
    impl BashOperations for RecordingOps {
        async fn exec(
            &self,
            request: BashExecRequest<'_>,
        ) -> Result<CommandResult, ExecutionError> {
            self.calls.lock().unwrap().push((
                request.command.to_string(),
                request.cwd.unwrap().to_path_buf(),
                request.timeout,
            ));
            if let Some(on_data) = request.on_data {
                *self.on_data_calls.lock().unwrap() += 1;
                on_data(b"partial");
            }
            Ok(self.result.clone())
        }
    }

    #[tokio::test]
    async fn injected_backend_receives_command_cwd_and_timeout() {
        let ops = Arc::new(RecordingOps {
            calls: std::sync::Mutex::new(Vec::new()),
            on_data_calls: std::sync::Mutex::new(0),
            result: CommandResult {
                stdout: "ok".into(),
                stderr: String::new(),
                exit_code: 0,
            },
        });
        let tool = BashTool::with_operations(Some("export A=1".into()), ops.clone());
        let result = tool
            .execute(
                "c1",
                serde_json::json!({"command": "echo hi", "timeout": 5000}),
                CancellationToken::new(),
                &ctx_with_env(Arc::new(MockEnv {
                    result: CommandResult {
                        stdout: String::new(),
                        stderr: String::new(),
                        exit_code: 0,
                    },
                })),
            )
            .await
            .unwrap();
        assert!(!result.is_error);

        let calls = ops.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        // The prefix is folded into the command before dispatch.
        assert_eq!(calls[0].0, "export A=1 echo hi");
        assert_eq!(calls[0].1, PathBuf::from("/mock"));
        assert_eq!(calls[0].2, Some(Duration::from_millis(5000)));
    }

    #[tokio::test]
    async fn injected_backend_streams_chunks_to_progress() {
        let ops = Arc::new(RecordingOps {
            calls: std::sync::Mutex::new(Vec::new()),
            on_data_calls: std::sync::Mutex::new(0),
            result: CommandResult {
                stdout: "ok".into(),
                stderr: String::new(),
                exit_code: 0,
            },
        });
        let tool = BashTool::with_operations(None, ops.clone());

        struct Sink;
        impl crate::tool::ToolProgress for Sink {
            fn emit(&self, _partial: JsonValue) {}
        }

        let _ = tool
            .execute_with_progress(
                "c1",
                serde_json::json!({"command": "echo hi"}),
                CancellationToken::new(),
                &ctx_with_env(Arc::new(MockEnv {
                    result: CommandResult {
                        stdout: String::new(),
                        stderr: String::new(),
                        exit_code: 0,
                    },
                })),
                &Sink,
            )
            .await
            .unwrap();

        assert_eq!(*ops.on_data_calls.lock().unwrap(), 1);
    }
}
