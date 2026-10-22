// Py tool — a short Python 3 script run on an embedded RustPython
// interpreter, standard library only.
//
// The interpreter is built per call and frozen with the binary
// (`freeze-stdlib`): no site-packages, no filesystem import path, nothing
// outside the standard library. The tool owns no sandbox of its own — the
// host gates the call on the session's permission mode — and it returns what
// the script itself produced: stdout and stderr share one captured stream
// (their interleaving is the transcript), an uncaught exception is the error
// face (formatted traceback plus whatever was printed before it), and
// `sys.exit(n)` is the script's exit status rather than a traceback.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::Value as JsonValue;
use tokio_util::sync::CancellationToken;

use rustpython::InterpreterBuilder;
use rustpython::InterpreterBuilderExt as _;
use rustpython::vm::{AsObject as _, VirtualMachine};

use crate::tool::{AgentTool, AgentToolResult, ExecutionMode, ToolContext, ToolError};
use crate::tools::truncate::{self, TruncateConfig};

/// The model-facing script budget. A Py script is a shortcut, not a program:
/// past this size the work belongs in a real file edited with the file tools
/// or in a Bash call, and the transcript is better for it.
const MAX_SCRIPT_LINES: usize = 120;

/// The preamble every script runs behind: stdout and stderr reroute into an
/// in-interpreter buffer (the process streams are never written — a print in
/// a script must land in the tool result, not the host's log), and `argv`
/// names the script exactly as the traceback does.
fn capture_preamble() -> String {
    "import sys, io as _manox_io\n\
     sys.argv = [\"<py>\"]\n\
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

/// Restores the process working directory when the call ends, including on a
/// panic or an early return — the script may have moved it, and the host's
/// other tools resolve their own absolute paths but must not inherit a
/// half-finished script's directory.
struct CwdGuard {
    previous: Option<PathBuf>,
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        let restored = self
            .previous
            .as_ref()
            .and_then(|dir| std::env::set_current_dir(dir).ok())
            .or_else(|| std::env::set_current_dir("/").ok());
        if restored.is_none() {
            tracing::warn!("the py tool could not restore the process working directory");
        }
    }
}

fn format_exception(
    vm: &VirtualMachine,
    exc: &rustpython::vm::builtins::PyBaseExceptionRef,
) -> String {
    let mut out = String::new();
    let _ = vm.write_exception(&mut out, exc);
    out
}

/// `sys.exit(code)` is a status, not a failure transcript: an int is the exit
/// code, `None` is 0, and any other value prints its string form and exits 1
/// (the CPython contract). Returns `None` when the exception is not
/// `SystemExit`.
fn system_exit_status(
    vm: &VirtualMachine,
    exc: &rustpython::vm::builtins::PyBaseExceptionRef,
) -> Option<(i32, String)> {
    if !exc.fast_isinstance(vm.ctx.exceptions.system_exit) {
        return None;
    }
    let code = exc.as_object().get_attr("code", vm).ok();
    match code {
        None => Some((0, String::new())),
        Some(value) if vm.ctx.none().is(&value) => Some((0, String::new())),
        Some(value) => match value.try_to_value::<i32>(vm) {
            Ok(code) => Some((code, String::new())),
            Err(_) => Some((1, value.str(vm).map(|s| s.to_string()).unwrap_or_default())),
        },
    }
}

/// One script, start to finish. Returns the captured output and the exit
/// status, or the error face — a compile error, an uncaught exception, or an
/// interpreter failure — as text with a failing status.
fn run_script(source: &str, cwd: &Path) -> (String, i32) {
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
        Ok(handle) => handle
            .join()
            .unwrap_or_else(|_| ("internal: the interpreter thread panicked".to_string(), 1)),
        Err(e) => (
            format!("internal: the interpreter thread failed to spawn: {e}"),
            1,
        ),
    }
}

fn run_interpreted(source: &str, cwd: &Path) -> (String, i32) {
    // A panic in a previous script poisons the lock; the directory restore it
    // guards already ran on unwind, so recovering the guard is safe and keeps
    // one bad script from disabling the tool.
    let _serialized = PY_CALL_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let previous = std::env::current_dir().ok();
    if let Err(e) = std::env::set_current_dir(cwd) {
        return (
            format!("internal: the working directory could not be entered: {e}"),
            1,
        );
    }
    let _cwd = CwdGuard { previous };
    run_on_interpreter(source)
}

fn run_on_interpreter(source: &str) -> (String, i32) {
    let interpreter = InterpreterBuilder::new().init_stdlib().build();
    interpreter.enter(|vm| {
        let scope = match vm.new_scope_with_main() {
            Ok(scope) => scope,
            Err(exc) => {
                return (
                    format!(
                        "internal: the main module could not be created: {}",
                        format_exception(vm, &exc)
                    ),
                    1,
                );
            }
        };
        if let Err(exc) = vm.run_string(scope.clone(), &capture_preamble(), "<py-setup>") {
            return (
                format!(
                    "internal: the capture setup failed: {}",
                    format_exception(vm, &exc)
                ),
                1,
            );
        }
        let outcome = vm.run_string(scope.clone(), source, "<py>");
        // The buffer is read even on a raising script: partial output plus
        // the traceback is the honest transcript of a failed run.
        let printed = vm
            .run_string(
                scope.clone(),
                "_manox_printed = _manox_out.getvalue()",
                "<py-teardown>",
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
        // print() ends its own newline; the buffer's trailing blank line is
        // harness noise, not script output.
        let printed = printed.trim_end_matches('\n').to_owned();
        match outcome {
            Ok(_) => (printed, 0),
            Err(exc) => {
                if let Some((code, message)) = system_exit_status(vm, &exc) {
                    let output = match (printed.is_empty(), message.is_empty()) {
                        (true, true) => String::new(),
                        (false, true) => printed,
                        (true, false) => message,
                        (false, false) => format!("{printed}\n{message}"),
                    };
                    return (output, code);
                }
                let traceback = format_exception(vm, &exc);
                (
                    if printed.is_empty() {
                        traceback
                    } else {
                        format!("{printed}\n{traceback}")
                    },
                    1,
                )
            }
        }
    })
}

pub struct PyTool;

impl PyTool {
    /// Default max bytes for output (matching the bash tool's budget).
    const DEFAULT_MAX_BYTES: usize = 128 * 1024;
    /// Default max lines for output.
    const DEFAULT_MAX_LINES: usize = 2000;
    /// Default script budget. The embedded interpreter cannot be preempted —
    /// a timed-out script keeps its blocking thread warm until it finishes —
    /// so the default is the bash tool's, and the model is told to keep
    /// scripts short.
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
        let line_count = script.lines().count();
        if line_count > MAX_SCRIPT_LINES {
            return Err(ToolError::InvalidArguments(format!(
                "script is {line_count} lines; the limit is {MAX_SCRIPT_LINES}. Split the work \
                 across calls, put it in a file and edit that with the file tools, or run it \
                 through Bash"
            )));
        }
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
        let (output, exit_code) = tokio::time::timeout(timeout, run)
            .await
            .map_err(|_| {
                ToolError::ExecutionFailed(format!(
                    "the script exceeded its {}s budget and was abandoned — it may still be \
                     running in the background; keep Py scripts short and move long work to Bash",
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
        if exit_code != 0 {
            if !final_output.is_empty() {
                final_output.push('\n');
            }
            final_output.push_str(&format!("[exit {exit_code}]"));
        }
        let mut result = AgentToolResult::text(final_output);
        result.is_error = exit_code != 0;
        Ok(result)
    }
}

#[async_trait::async_trait]
impl AgentTool for PyTool {
    fn name(&self) -> &str {
        "Py"
    }

    fn description(&self) -> &str {
        "Interpret and run a short Python 3 script in this process.\n\
         The standard library is built in and frozen: no site-packages, pip, or venv, and \
         no TLS, so HTTPS clients are unavailable. Whatever the script prints is the \
         result; an uncaught exception returns the traceback and marks the call failed.\n\
         \n\
         Use Read/Write/Edit/Grep/Glob/Ls for ordinary filesystem reads, writes, and \
         searches — reach for Py only when a script is clearly more efficient or more \
         expressive than those tools.\n\
         \n\
         For installed third-party packages (PIL, PyYAML, numpy), HTTPS requests, calling \
         other programs (git, cargo, gh), pipelines, background work, or inputs over \
         10 MB, use Bash with `python3` on the OS, provided it is safe to do so.\n\
         \n\
         Keep scripts short and safe — this is arbitrary code running inside the app \
         process. `os.chdir` has side effects: it really moves the conversation's CWD. \
         Be deliberate about runtime; avoid very long-running scripts unless you know and \
         accept the cost. Print a short summary, never a whole file, or the context will \
         explode. Keep scripts to at most 120 lines. Python's `re` supports capturing groups \
         but not lookbehind, lookahead, or backreferences."
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn execution_mode(&self) -> ExecutionMode {
        // The call moves the process's working directory and holds the
        // interpreter lock; a parallel batch would interleave with it.
        ExecutionMode::Sequential
    }

    fn parameters_schema(&self) -> JsonValue {
        serde_json::json!({
            "type": "object",
            "properties": {
                "script": {
                    "type": "string",
                    "description": "The Python 3 source to run (at most 120 lines). Standard \
                                    library only — no site-packages, no TLS. print() \
                                    is how output is returned."
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
            "required": ["script", "cwd"]
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
