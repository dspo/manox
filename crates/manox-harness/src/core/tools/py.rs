// Py tool — a short Python 3 script run on an embedded RustPython
// interpreter, standard library only.
//
// The interpreter is built per call and frozen with the binary
// (`freeze-stdlib`): no site-packages, no filesystem import path, nothing
// outside the standard library — the boundary the tool is documented to
// the model with. Output assembles exactly like [`BashTool`]'s (stdout,
// a `[stderr]` block, truncation), except an uncaught exception is the
// error face: the formatted traceback plus whatever was printed before it,
// and `is_error` set so the transcript paints it as a failure.

use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::Value as JsonValue;
use tokio_util::sync::CancellationToken;

use rustpython::InterpreterBuilder;
use rustpython::InterpreterBuilderExt as _;
use rustpython::vm::VirtualMachine;

use crate::tool::{AgentTool, AgentToolResult, ToolContext, ToolError};
use crate::tools::truncate::{self, TruncateConfig};

/// The preamble every script runs behind: stdout and stderr reroute into an
/// in-interpreter buffer (the process streams are never written — a print
/// in a script must land in the tool result, not the host's log), and the
/// call's working directory becomes the script's cwd (the embedded os module
/// would otherwise read the host process's directory).
fn capture_preamble() -> String {
    "import sys, io as _manox_io\n\
     _manox_out = _manox_io.StringIO()\n\
     sys.stdout = _manox_out\n\
     sys.stderr = _manox_out\n"
        .to_string()
}

/// Serializes every Py call: the embedded interpreter's `os.chdir` is a
/// PROCESS-wide syscall (there is no per-interpreter directory), so the
/// call's working directory is applied by actually moving the process and
/// restoring it afterwards — two scripts running concurrently would stomp
/// each other, and the restore races nothing because the lock is held
/// across the whole script.
static PY_CALL_LOCK: Mutex<()> = Mutex::new(());

fn format_exception(
    vm: &VirtualMachine,
    exc: &rustpython::vm::builtins::PyBaseExceptionRef,
) -> String {
    let mut out = String::new();
    let _ = vm.write_exception(&mut out, exc);
    out
}

/// One script, start to finish. Returns the captured output, or the error
/// face — a compile error, an uncaught exception, or an interpreter failure —
/// as text.
fn run_script(source: &str, cwd: &Path) -> (String, bool) {
    // The frozen importlib bootstrap recurses deeply before a single user
    // statement runs, and a user script can recurse deeper still; the
    // overflow surfaces as a guard-page SIGILL — not a catchable error — on
    // whatever stack the caller offered. The interpreter gets its own thread
    // with an explicit, generous stack (a virtual reservation, not commit).
    let source = source.to_string();
    let cwd = cwd.to_path_buf();
    let spawned = std::thread::Builder::new()
        .name("py-tool".into())
        .stack_size(64 * 1024 * 1024)
        .spawn(move || run_interpreted(&source, &cwd));
    match spawned {
        Ok(handle) => handle.join().unwrap_or_else(|_| {
            (
                "internal: the interpreter thread panicked".to_string(),
                true,
            )
        }),
        Err(e) => (
            format!("internal: the interpreter thread failed to spawn: {e}"),
            true,
        ),
    }
}

fn run_interpreted(source: &str, cwd: &Path) -> (String, bool) {
    let _serialized = PY_CALL_LOCK.lock();
    let previous = std::env::current_dir().ok();
    if let Err(e) = std::env::set_current_dir(cwd) {
        return (
            format!("internal: the working directory could not be entered: {e}"),
            true,
        );
    }
    let ran = run_on_interpreter(source);
    // Restore even after a raising script; "/" is the honest last resort.
    let restored = previous
        .and_then(|dir| std::env::set_current_dir(dir).ok())
        .or_else(|| std::env::set_current_dir("/").ok());
    if restored.is_none() {
        tracing::warn!("the py tool could not restore the process working directory");
    }
    ran
}

fn run_on_interpreter(source: &str) -> (String, bool) {
    let interpreter = InterpreterBuilder::new().init_stdlib().build();
    interpreter.enter(|vm| {
        let scope = vm.new_scope_with_builtins();
        if let Err(exc) = vm.run_string(scope.clone(), &capture_preamble(), "<py-setup>".into()) {
            return (
                format!(
                    "internal: the capture setup failed: {}",
                    format_exception(vm, &exc)
                ),
                true,
            );
        }
        let failed = vm
            .run_string(scope.clone(), source, "<py>".into())
            .err()
            .map(|exc| format_exception(vm, &exc));
        // The buffer is read even on a raising script: partial output plus
        // the traceback is the honest transcript of a failed run.
        let printed = vm
            .run_string(
                scope.clone(),
                "_manox_printed = _manox_out.getvalue()",
                "<py-teardown>".into(),
            )
            .is_ok()
            .then(|| {
                scope
                    .globals
                    .get_item_opt("_manox_printed", vm)
                    .ok()
                    .flatten()
                    .and_then(|value| value.str(vm).ok().map(|s| s.to_string()))
            })
            .flatten()
            .unwrap_or_default();
        match failed {
            // print() ends its own newline; the buffer's trailing blank line
            // is harness noise, not script output.
            None => (printed.trim_end_matches('\n').to_owned(), false),
            Some(traceback) => (
                if printed.is_empty() {
                    traceback
                } else {
                    format!("{}\n{traceback}", printed.trim_end_matches('\n'))
                },
                true,
            ),
        }
    })
}

pub struct PyTool;

impl PyTool {
    /// Default max bytes for output (matching the bash tool's budget).
    const DEFAULT_MAX_BYTES: usize = 128 * 1024;
    /// Default max lines for output.
    const DEFAULT_MAX_LINES: usize = 2000;
    /// The tool's cwd is applied as a real (process-level) chdir, serialized
    /// across calls and restored afterwards — see [`PY_CALL_LOCK`].
    ///
    /// Default script budget. The embedded interpreter cannot be preempted —
    /// a timed-out script keeps its blocking thread warm until it finishes —
    /// so the default is the bash tool's: generous for short scripts, and
    /// the model is told to keep them short.
    const DEFAULT_TIMEOUT_MS: u64 = 120_000;

    async fn run(
        &self,
        params: JsonValue,
        signal: CancellationToken,
        ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        let script = params["script"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("script is required".into()))?;
        let timeout = Duration::from_millis(
            params["timeout"]
                .as_u64()
                .unwrap_or(Self::DEFAULT_TIMEOUT_MS),
        );
        let cwd = crate::tools::path_utils::resolve_effective_cwd(ctx, params.get("cwd"))
            .map_err(ToolError::InvalidArguments)?;
        if !cwd.is_dir() {
            return Err(ToolError::InvalidArguments(format!(
                "the working directory does not exist: {}",
                cwd.display()
            )));
        }
        let script = script.to_string();
        let run = tokio::task::spawn_blocking(move || run_script(&script, &cwd));
        let (output, is_error) = tokio::time::timeout(timeout, run)
            .await
            .map_err(|_| {
                ToolError::ExecutionFailed(format!(
                    "the script exceeded its {}s budget and was abandoned; \
                     keep Py scripts short — split the work across calls",
                    timeout.as_secs()
                ))
            })?
            .map_err(|e| ToolError::ExecutionFailed(format!("the interpreter task failed: {e}")))?;
        if signal.is_cancelled() {
            return Err(ToolError::Aborted);
        }

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
        let mut result = AgentToolResult::text(final_output);
        result.is_error = is_error;
        Ok(result)
    }
}

#[async_trait::async_trait]
impl AgentTool for PyTool {
    fn name(&self) -> &str {
        "Py"
    }

    fn description(&self) -> &str {
        "Run a short Python 3 script string on an embedded interpreter \
         (standard library only) and return what it printed"
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
                "script": {
                    "type": "string",
                    "description": "The Python 3 source to run. Standard library only — \
                                    there are no site-packages, no shell, and no network TLS. \
                                    print() is how output is returned."
                },
                "cwd": {
                    "type": "array",
                    "items": { "type": "string" },
                    "minItems": 1,
                    "description": crate::tools::path_utils::CWD_SCHEMA_DOC
                },
                "timeout": {
                    "type": "integer",
                    "description": "Timeout in milliseconds (default: 120000)"
                }
            },
            "required": ["script"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: JsonValue,
        signal: CancellationToken,
        ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        self.run(params, signal, ctx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::ExecutionEnv;
    use crate::tool::{LocalToolContext, ToolState};
    use std::path::Path;
    use std::path::PathBuf;
    use std::sync::Arc;

    struct MockEnv;

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
        ) -> Result<crate::env::CommandResult, crate::env::ExecutionError> {
            unreachable!("the Py tool never shells out")
        }
        async fn exec_at(
            &self,
            _command: &str,
            _cwd: &Path,
            _timeout: Duration,
            _signal: CancellationToken,
        ) -> Result<crate::env::CommandResult, crate::env::ExecutionError> {
            unreachable!("the Py tool never shells out")
        }
    }

    fn ctx() -> (LocalToolContext, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let context = LocalToolContext::new(
            Arc::new(MockEnv),
            dir.path().to_path_buf(),
            Arc::new(ToolState::new()),
        );
        (context, dir)
    }

    fn text(result: &AgentToolResult) -> String {
        match &result.content[0] {
            crate::types::ContentBlock::Text { text, .. } => text.clone(),
            _ => panic!("expected a text block"),
        }
    }

    #[tokio::test]
    async fn a_script_prints_into_the_result() {
        let (context, _dir) = ctx();
        let result = PyTool
            .execute(
                "c1",
                serde_json::json!({
                    "script": "print('hello from rustpython')\nprint(sum(range(10)))",
                    "cwd": [""]
                }),
                CancellationToken::new(),
                &context,
            )
            .await
            .unwrap();
        assert!(!result.is_error);
        assert_eq!(text(&result), "hello from rustpython\n45");
    }

    /// The tool's boundary: the standard library is importable, the process
    /// streams never receive a print, and the call's cwd is the script's cwd.
    #[tokio::test]
    async fn the_script_runs_inside_the_call_directory_with_captured_output() {
        let (context, dir) = ctx();
        let result = PyTool
            .execute(
                "c1",
                serde_json::json!({
                    "script": "import os, json\nprint(os.getcwd())\nprint(json.dumps({'k': [1, 2]}))",
                    "cwd": [""]
                }),
                CancellationToken::new(),
                &context,
            )
            .await
            .unwrap();
        assert!(!result.is_error, "out: {}", text(&result));
        let expected = format!(
            "{}\n{{\"k\": [1, 2]}}",
            dir.path()
                .canonicalize()
                .expect("the tempdir resolves")
                .display()
        );
        assert_eq!(text(&result), expected);
    }

    /// An uncaught exception is the error face: the traceback names the
    /// failing line of the SCRIPT (not a wrapper), and output printed before
    /// the raise is preserved.
    #[tokio::test]
    async fn an_exception_fails_with_a_traceback_and_partial_output() {
        let (context, _dir) = ctx();
        let result = PyTool
            .execute(
                "c1",
                serde_json::json!({
                    "script": "print('before')\nraise ValueError('boom')",
                    "cwd": [""]
                }),
                CancellationToken::new(),
                &context,
            )
            .await
            .unwrap();
        let out = text(&result);
        assert!(result.is_error, "the exception is an error result: {out}");
        assert!(out.starts_with("before\n"), "partial output first: {out}");
        assert!(
            out.contains("ValueError: boom"),
            "the traceback is in: {out}"
        );
        assert!(
            out.contains("<py>"),
            "the failing frame names the script: {out}"
        );
    }

    #[tokio::test]
    async fn missing_script_is_an_argument_error() {
        let (context, _dir) = ctx();
        let err = PyTool
            .execute(
                "c1",
                serde_json::json!({"cwd": [""]}),
                CancellationToken::new(),
                &context,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    /// The approval gate: a tool that runs arbitrary code is never silent.
    #[test]
    fn scripts_require_approval() {
        assert!(PyTool.requires_approval(&serde_json::json!({"script": "1"})));
        assert!(!PyTool.is_read_only());
        assert_eq!(PyTool.name(), "Py");
    }
}
