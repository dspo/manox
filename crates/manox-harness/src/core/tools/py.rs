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

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
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
/// a script must land in the tool result, not the host's log), `argv` names
/// the script exactly as the traceback does, and the recursion limit is
/// frozen where the interpreter left it — raising it is the one script-side
/// lever onto the interpreter's stack, and an overflow is a guard-page SIGILL
/// that takes the whole process, not a catchable `RecursionError`.
fn capture_preamble() -> String {
    let mut source = [
        "import sys, io as _manox_io",
        "sys.argv = [\"<py>\"]",
        "_manox_out = _manox_io.StringIO()",
        "sys.stdout = _manox_out",
        "sys.stderr = _manox_out",
        "_manox_setrecursionlimit = sys.setrecursionlimit",
        "def _manox_recursionlimit(limit):",
        "    if limit > sys.getrecursionlimit():",
        "        raise RuntimeError('the recursion limit is capped; keep recursion shallow')",
        "    _manox_setrecursionlimit(limit)",
        "sys.setrecursionlimit = _manox_recursionlimit",
    ]
    .join("\n");
    source.push('\n');
    source
}

/// Serializes every Py call: the embedded interpreter's `os.chdir` is a
/// PROCESS-wide syscall (there is no per-interpreter directory), so the
/// call's working directory is applied by actually moving the process and
/// restoring it afterwards — two scripts running concurrently would stomp
/// each other. The slot is claimed by the caller *before* any thread is
/// spawned and released by the script's thread when it ends (including on
/// a panic unwind), so it stays claimed for the script's whole lifetime
/// *including a timeout abandonment*: a new call arriving while any script
/// is live fails fast instead of piling another stack-hungry thread onto a
/// mutex — a wedged script cannot be preempted, and the tool answers "a
/// previous script is still running" until its thread ends.
///
/// The process cwd points at the script's directory for as long as the
/// script runs (including the abandoned tail). A per-interpreter cwd would
/// need every relative-path primitive (`open`, `listdir`, `Path`) patched
/// to resolve against a per-call base — shimming only `os.getcwd`/`os.chdir`
/// would split what `getcwd()` reports from where `open('rel')` actually
/// reads — so the process move stays, and process-cwd readers (e.g. MCP
/// project-config discovery) see the script's directory during that window
/// by design of this tool.
static SCRIPT_SLOT: AtomicBool = AtomicBool::new(false);

/// Releases the call slot when the script's thread ends, on return or on a
/// panic unwind alike.
struct SlotRelease;

impl Drop for SlotRelease {
    fn drop(&mut self) {
        SCRIPT_SLOT.store(false, Ordering::Release);
    }
}

/// Puts the process working directory back where the call found it. Only a
/// call that entered without a readable `previous` (getcwd itself failed)
/// falls back to `/`; a failed restore of a real directory keeps its
/// warning — silently relocating the process to `/` after the script
/// deleted its own directory would misdirect every later process-cwd
/// consumer without a trace.
fn restore_process_cwd(previous: Option<PathBuf>) {
    match previous {
        Some(dir) => {
            if let Err(e) = std::env::set_current_dir(&dir) {
                tracing::warn!(
                    error = %e,
                    dir = %dir.display(),
                    "the py tool could not restore the process working directory; falling back to /"
                );
                let _ = std::env::set_current_dir("/");
            }
        }
        None => {
            let _ = std::env::set_current_dir("/");
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

/// One script, start to finish, on the caller's terms: the caller has
/// already claimed the call slot and entered the working directory (the
/// slot's release rides this thread). Returns the captured output and the
/// exit status, or the error face — a compile error, an uncaught exception,
/// or an interpreter failure — as text with a failing status.
fn run_script(source: &str) -> (String, i32) {
    // The frozen importlib bootstrap recurses deeply before a single user
    // statement runs, and a user script can recurse deeper still; the
    // overflow surfaces as a guard-page SIGILL — not a catchable error — on
    // whatever stack the caller offered. The interpreter gets its own thread
    // with an explicit, generous stack (a virtual reservation, not commit).
    // The preamble caps the script-side recursion limit, so raising it is
    // not a lever; a pathological deeply-nested *parse* still overflows
    // here and takes the process — the accepted residual of an in-process
    // interpreter (a child process is the only hard bound).
    let _release = SlotRelease;
    let source = source.to_string();
    let spawned = std::thread::Builder::new()
        .name("py-tool".into())
        .stack_size(64 * 1024 * 1024)
        .spawn(move || run_on_interpreter(&source));
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
        // One script at a time: the slot is claimed here, before any thread
        // is spawned, and released by the script's thread when it ends — an
        // abandoned script keeps it claimed, so a call arriving behind a
        // wedged script fails here instead of stacking another 64MB-stack
        // thread that would only burn its own budget waiting.
        if SCRIPT_SLOT.swap(true, Ordering::AcqRel) {
            return Err(ToolError::ExecutionFailed(
                "a previous script is still running (a timed-out script cannot be \
                 preempted); the tool stays unavailable until it finishes — move the \
                 work to Bash"
                    .into(),
            ));
        }
        let previous = std::env::current_dir().ok();
        if let Err(e) = std::env::set_current_dir(&cwd) {
            return Err(ToolError::InvalidArguments(format!(
                "the working directory could not be entered: {e}"
            )));
        }
        let run = tokio::task::spawn_blocking(move || run_script(&script));
        let joined = tokio::time::timeout(timeout, run).await;
        // The directory is restored the moment the budget ends, not when the
        // abandoned script eventually finishes — a wedged script must not
        // keep the process parked in its directory. (Its own `os.chdir`
        // during the abandoned tail can still move the process; the slot
        // stays claimed until that tail ends, so no later Py call builds on
        // the mess.)
        restore_process_cwd(previous);
        let (output, exit_code) = joined
            .map_err(|_| {
                ToolError::ExecutionFailed(format!(
                    "the script exceeded its {}s budget and was abandoned — it cannot be \
                     preempted and still holds the tool, so later Py calls are refused until \
                     it finishes; keep Py scripts short and move long work to Bash",
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

    fn requires_approval(&self, _params: &JsonValue) -> bool {
        // A script's effects are decided at runtime — there is no
        // params-aware fence to consult (unlike Bash's sandbox/confined
        // split). The declarative hint is what the subagent `never` gate
        // rejects on: an in-process interpreter with no OS sandbox cannot
        // ride a child session, so a subagent's Py call settles to the
        // denial instead of executing unfenced.
        true
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
            // `cwd` is deliberately not required: a missing cwd passes schema
            // validation and is rejected at resolution with the instructional
            // CWD_REQUIRED_DOC text (the shared contract of every cwd tool).
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
