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
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use crate::sandbox::{
    ESCALATION_TARGETS, EscalationApprover, EscalationRequest, NO_GRANT, PermissionMode,
    approve_escalation, validate_escalation_args,
};
use pi::BackgroundTaskRegistry;
use pi::env::CommandResult;

use orchestration::{BackgroundManager, OutputShape};
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
/// execution via the registry, head/tail line filters, and a
/// `sandbox_permissions` escalation slot. The per-call effective mode
/// (read-only / workspace-write / danger-full-access) selects the backend:
/// `danger-full-access` runs through `unsandboxed_operations` (no
/// confinement); `read-only` / `workspace-write` ride the sandboxed
/// `operations`, whose seatbelt profile the mode resolves. A one-shot
/// escalation widens the effective mode for a single call after a
/// host-injected approval round-trip.
pub struct BashTool {
    operations: Arc<dyn BashOperations>,
    registry: Arc<dyn BackgroundTaskRegistry>,
    command_prefix: Option<String>,
    /// Optional orchestrator: when bound, background tasks are registered,
    /// watched, and their completions steered into the agent session.
    manager: Option<Arc<BackgroundManager>>,
    /// Backend used when the effective mode is `danger-full-access`
    /// (host-forced, or an approved escalation grant). Absent, such calls
    /// fall back to the default backend.
    unsandboxed_operations: Option<Arc<dyn BashOperations>>,
    /// Host-injected session-mode resolver. Returns the standing mode the
    /// call runs under absent an escalation grant.
    mode_resolver: Option<Arc<dyn Fn() -> PermissionMode + Send + Sync>>,
    /// Host-injected approval channel for a `sandbox_permissions` escalation.
    /// Absent, escalation fails closed ("no approval service composed").
    escalation_approver: Option<Arc<dyn EscalationApprover + Send + Sync>>,
    /// Shared per-call grant cell: a one-shot escalation stamp the sandboxed
    /// backend's mode resolver reads before the standing mode. `NO_GRANT`
    /// means no grant. Sequential execution makes the set-then-read safe.
    grant_cell: Option<Arc<AtomicI64>>,
    /// Whether an OS sandbox backend is installed. `false` (Linux/Windows)
    /// means no seatbelt to confine bash, so every call needs approval and
    /// background tasks stay bare.
    sandbox_available: bool,
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
            unsandboxed_operations: None,
            mode_resolver: None,
            escalation_approver: None,
            grant_cell: None,
            sandbox_available: false,
        }
    }

    /// Prepend a command to every invocation (shell setup commands).
    pub fn with_command_prefix(mut self, prefix: Option<String>) -> Self {
        self.command_prefix = prefix;
        self
    }

    /// Bind the unsandboxed backend: calls whose effective mode is
    /// `danger-full-access` run through this backend instead of the
    /// sandboxed one. When the host installs no second backend, such calls
    /// keep the default backend.
    pub fn with_unsandboxed_operations(mut self, ops: Arc<dyn BashOperations>) -> Self {
        self.unsandboxed_operations = Some(ops);
        self
    }

    /// Bind the session-mode resolver (the standing mode absent an
    /// escalation grant). Hosts wire this to their `ApprovalGate::mode()`.
    pub fn with_mode_resolver(
        mut self,
        resolver: Arc<dyn Fn() -> PermissionMode + Send + Sync>,
    ) -> Self {
        self.mode_resolver = Some(resolver);
        self
    }

    /// Bind the escalation approval channel. Required when a sandbox backend
    /// is mounted and the model may pass `sandbox_permissions`; absent,
    /// escalation fails closed.
    pub fn with_escalation_approver(
        mut self,
        approver: Arc<dyn EscalationApprover + Send + Sync>,
    ) -> Self {
        self.escalation_approver = Some(approver);
        self
    }

    /// Bind the per-call grant cell, shared with the sandboxed backend's
    /// mode resolver. The tool stamps an approved grant here for one call;
    /// the resolver reads it before the standing mode.
    pub fn with_grant_cell(mut self, cell: Arc<AtomicI64>) -> Self {
        self.grant_cell = Some(cell);
        self
    }

    /// Declare whether an OS sandbox backend confines this tool. When false
    /// (no seatbelt on the platform), every call requires approval — there
    /// is no OS confinement to stand in for the gate — and background tasks
    /// never route through a sandbox wrapper.
    pub fn with_sandbox_available(mut self, available: bool) -> Self {
        self.sandbox_available = available;
        self
    }

    /// Bind an orchestrator so background tasks participate in the agent
    /// session's lifecycle. The manager's registry replaces the wrapper's
    /// own, so tasks it spawns stay visible to `BashOutput` / `TaskStop`.
    pub fn with_manager(mut self, manager: Arc<BackgroundManager>) -> Self {
        self.registry = manager.registry.clone();
        self.manager = Some(manager);
        self
    }
}

#[async_trait::async_trait]
impl AgentTool for BashTool {
    fn name(&self) -> &str {
        "Bash"
    }

    fn description(&self) -> &str {
        "Execute a shell command. State (cwd, exported vars, functions) persists across calls. \
         Optionally run in the background with `run_in_background: true` — the command starts in a \
         fresh shell (no persistent state) at the session cwd — a completion summary with the \
         output tail arrives automatically; fetch the full output via `BashOutput`; stop with \
         `TaskStop`. Use `head_lines`/`tail_lines` to keep a selection of the output instead of \
         piping through `head`/`tail`. Commands run under a file sandbox; a blocked file operation \
         is reported as `[sandbox: file access denied under <mode> mode]` — a policy denial, not a \
         bug. When a command is denied and a wider mode would let it succeed, retry the exact same \
         command once with `sandbox_permissions` (the narrowest wider mode that suffices) plus a \
         one-sentence `justification`; the approval prompt asks the user. Never escalate \
         speculatively — ground the request in a real denial."
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn requires_approval(&self, params: &JsonValue) -> bool {
        // No OS sandbox (Linux/Windows) → no confinement to stand in for
        // the gate: every call needs approval. With a seatbelt, confined
        // calls ride the OS confinement (no approval); only a
        // `sandbox_permissions` escalation needs the approval round-trip —
        // the escalation itself is the sensitive act.
        if !self.sandbox_available {
            return true;
        }
        params
            .get("sandbox_permissions")
            .and_then(|v| v.as_str())
            .is_some()
    }

    /// The persistent shell is stateful: a batch of parallel bash calls
    /// would interleave their mutations.
    fn execution_mode(&self) -> pi::tool::ExecutionMode {
        pi::tool::ExecutionMode::Sequential
    }

    fn parameters_schema(&self) -> JsonValue {
        let mut properties = serde_json::Map::new();
        properties.insert(
            "command".into(),
            serde_json::json!({"type": "string", "description": "The command to execute"}),
        );
        properties.insert(
            "timeout".into(),
            serde_json::json!({"type": "integer", "description": "Timeout in milliseconds (default: 120000)"}),
        );
        properties.insert(
            "cwd".into(),
            serde_json::json!({"type": "string", "description": "Working directory; defaults to the session cwd. The shell's cwd persists across calls"}),
        );
        properties.insert(
            "run_in_background".into(),
            serde_json::json!({"type": "boolean", "description": "Start the command in the background and return a task id immediately; a completion summary with the output tail arrives automatically — use `BashOutput` for the full output, `TaskStop` to stop"}),
        );
        properties.insert(
            "head_lines".into(),
            serde_json::json!({"type": "integer", "description": "Keep only the first N lines of output (applies to the result and the background completion summary)"}),
        );
        properties.insert(
            "tail_lines".into(),
            serde_json::json!({"type": "integer", "description": "Keep only the last N lines of output (applies to the result and the background completion summary)"}),
        );
        // The escalation fields are advertised only when a sandbox backend is
        // mounted — without one there is no confinement to widen, and a retry
        // can only fail closed.
        if self.sandbox_available {
            properties.insert(
                "sandbox_permissions".into(),
                serde_json::json!({
                    "type": "string",
                    "enum": ESCALATION_TARGETS.iter().map(|m| m.wire()).collect::<Vec<_>>(),
                    "description": "The wider sandbox mode this command needs. Only valid as a one-shot retry of a command the sandbox just denied; requires justification and user approval."
                }),
            );
            properties.insert(
                "justification".into(),
                serde_json::json!({"type": "string", "description": "Required with sandbox_permissions: one sentence for the user explaining why this exact command needs the wider access."}),
            );
        }
        serde_json::json!({
            "type": "object",
            "properties": properties,
            "required": ["command"]
        })
    }

    async fn execute(
        &self,
        tool_call_id: &str,
        params: JsonValue,
        signal: CancellationToken,
        ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        self.run(tool_call_id, params, signal, ctx, None).await
    }

    async fn execute_with_progress(
        &self,
        tool_call_id: &str,
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
        self.run(tool_call_id, params, signal, ctx, Some(&on_data))
            .await
    }
}

impl BashTool {
    async fn run(
        &self,
        tool_call_id: &str,
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

        // Resolve a one-shot sandbox-escalation grant through the host
        // approval channel BEFORE anything executes. The standing session
        // mode is the strict-wider baseline; an approved grant widens the
        // effective mode for exactly this call (stamped on the shared grant
        // cell, which the sandboxed backend's mode resolver reads). The
        // guard clears the stamp on drop so the next call reverts.
        let standing = self.standing_mode();
        let _grant =
            EscalationGrant::resolve(self, tool_call_id, &signal, &params, standing).await?;
        let effective = self.effective_mode();

        if run_in_background {
            // Background tasks route through the same confinement decision
            // as foreground calls: danger-full-access → bare spawn, else
            // sandboxed. Without a seatbelt there is no wrapper either way.
            let sandboxed = self.sandbox_available && effective != PermissionMode::DangerFullAccess;
            let shape = OutputShape {
                head_lines,
                tail_lines,
            };
            let id = match &self.manager {
                Some(manager) if sandboxed => manager
                    .spawn_sandboxed(&command, run_cwd, shape)
                    .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?,
                Some(manager) => manager
                    .spawn(&command, run_cwd, shape)
                    .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?,
                None => self
                    .registry
                    .spawn(&command, run_cwd)
                    .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?,
            };
            return Ok(AgentToolResult::text(format!(
                "Started in background as `{id}`. A completion summary with the output tail will arrive automatically. Use `BashOutput` (shell_id) for the full output; `TaskStop` stops the task."
            )));
        }

        let backend = if effective == PermissionMode::DangerFullAccess {
            self.unsandboxed_operations
                .as_deref()
                .unwrap_or(&*self.operations)
        } else {
            &*self.operations
        };
        let result = backend
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

    /// The standing session mode (absent any per-call grant).
    fn standing_mode(&self) -> PermissionMode {
        self.mode_resolver.as_ref().map(|r| r()).unwrap_or_default()
    }

    /// The per-call effective mode: an approved escalation grant stamped on
    /// the shared cell, else the standing session mode.
    fn effective_mode(&self) -> PermissionMode {
        if let Some(cell) = &self.grant_cell {
            let g = cell.load(Ordering::SeqCst);
            if g != NO_GRANT {
                return PermissionMode::from_i64(g);
            }
        }
        self.standing_mode()
    }
}

/// A one-shot sandbox-escalation grant: validates the argument pairing,
/// resolves the wider mode through the host approval channel, stamps it on
/// the shared grant cell for exactly this call, and clears the stamp on drop
/// so the next call reverts to the standing mode.
struct EscalationGrant<'a> {
    cell: Option<&'a Arc<AtomicI64>>,
}

impl<'a> EscalationGrant<'a> {
    async fn resolve(
        tool: &'a BashTool,
        tool_call_id: &str,
        signal: &CancellationToken,
        params: &JsonValue,
        standing: PermissionMode,
    ) -> Result<Self, ToolError> {
        let sp = params.get("sandbox_permissions").and_then(|v| v.as_str());
        let just = params.get("justification").and_then(|v| v.as_str());
        validate_escalation_args(sp, just).map_err(ToolError::InvalidArguments)?;
        let guard = Self {
            cell: tool.grant_cell.as_ref(),
        };
        if let Some(requested) = sp {
            let requested = PermissionMode::from_wire(requested).ok_or_else(|| {
                ToolError::InvalidArguments(format!(
                    "sandbox_permissions must be one of: {}",
                    ESCALATION_TARGETS
                        .iter()
                        .map(|m| m.wire())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })?;
            let approver = tool.escalation_approver.as_ref().ok_or_else(|| {
                ToolError::Other(
                    "sandbox escalation requires approval, but no approval service is composed"
                        .into(),
                )
            })?;
            let grant = approve_escalation(
                EscalationRequest {
                    requested_mode: requested,
                    justification: just.unwrap().to_string(),
                    effective_mode: standing,
                    subject: "command".into(),
                    tool_name: "Bash".into(),
                    call_id: tool_call_id.to_string(),
                    signal: Some(signal.clone()),
                },
                Some(approver.as_ref()),
            )
            .await
            .map_err(ToolError::Other)?;
            if let Some(cell) = &guard.cell {
                cell.store(grant.as_i64(), Ordering::SeqCst);
            }
        }
        Ok(guard)
    }
}

impl Drop for EscalationGrant<'_> {
    fn drop(&mut self) {
        if let Some(cell) = self.cell {
            cell.store(NO_GRANT, Ordering::SeqCst);
        }
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

    use super::background::BackgroundRegistry;

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

    /// Recording backend that tags each call with its identity so tests can
    /// assert which backend handled an invocation.
    struct TaggedOps {
        tag: &'static str,
        calls: Mutex<Vec<String>>,
    }
    #[async_trait::async_trait]
    impl BashOperations for TaggedOps {
        async fn exec(
            &self,
            _request: BashExecRequest<'_>,
        ) -> Result<CommandResult, pi::env::ExecutionError> {
            self.calls.lock().unwrap().push(self.tag.to_string());
            Ok(CommandResult {
                stdout: format!("ran={}", self.tag),
                stderr: String::new(),
                exit_code: 0,
            })
        }
    }

    fn output_text(result: &AgentToolResult) -> String {
        match &result.content[0] {
            pi::types::ContentBlock::Text { text, .. } => text.clone(),
            _ => panic!("expected text"),
        }
    }

    #[tokio::test]
    async fn default_calls_ride_the_default_backend() {
        let sandboxed = Arc::new(TaggedOps {
            tag: "sandboxed",
            calls: Mutex::new(Vec::new()),
        });
        let unsandboxed = Arc::new(TaggedOps {
            tag: "unsandboxed",
            calls: Mutex::new(Vec::new()),
        });
        let tool = BashTool::new(sandboxed.clone(), Arc::new(NoopRegistry))
            .with_unsandboxed_operations(unsandboxed.clone());
        let result = tool
            .execute(
                "c1",
                serde_json::json!({"command": "echo hi"}),
                CancellationToken::new(),
                &ctx("/base"),
            )
            .await
            .unwrap();
        assert!(output_text(&result).contains("ran=sandboxed"));
        assert_eq!(unsandboxed.calls.lock().unwrap().len(), 0);
    }

    /// A canned escalation approver: returns one fixed outcome, and records
    /// that it was asked (so a non-widening request can prove it never
    /// prompted).
    struct CannedApprover {
        outcome: crate::sandbox::EscalationOutcome,
        asked: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl crate::sandbox::EscalationApprover for CannedApprover {
        async fn request(
            &self,
            _req: crate::sandbox::EscalationRequest,
        ) -> crate::sandbox::EscalationOutcome {
            self.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.outcome
        }
    }

    fn escalation_tool(
        standing: PermissionMode,
        approver: Option<Arc<CannedApprover>>,
    ) -> (BashTool, Arc<std::sync::atomic::AtomicUsize>) {
        let sandboxed = Arc::new(TaggedOps {
            tag: "sandboxed",
            calls: Mutex::new(Vec::new()),
        });
        let unsandboxed = Arc::new(TaggedOps {
            tag: "unsandboxed",
            calls: Mutex::new(Vec::new()),
        });
        let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let grant_cell = Arc::new(AtomicI64::new(NO_GRANT));
        let standing_cell = Arc::new(AtomicI64::new(standing.as_i64()));
        let resolver: Arc<dyn Fn() -> PermissionMode + Send + Sync> = {
            let sc = Arc::clone(&standing_cell);
            Arc::new(move || PermissionMode::from_i64(sc.load(Ordering::SeqCst)))
        };
        let mut tool = BashTool::new(sandboxed, Arc::new(NoopRegistry))
            .with_unsandboxed_operations(unsandboxed)
            .with_sandbox_available(true)
            .with_mode_resolver(resolver)
            .with_grant_cell(grant_cell);
        if let Some(ap) = approver {
            tool = tool.with_escalation_approver(ap);
        }
        (tool, asked)
    }

    #[tokio::test]
    async fn sandbox_permissions_grants_a_wider_mode_for_one_call() {
        // Standing read-only; an approved escalation to danger-full-access
        // routes the call through the unsandboxed backend.
        let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let approver = Arc::new(CannedApprover {
            outcome: crate::sandbox::EscalationOutcome::AllowedOnce,
            asked: Arc::clone(&asked),
        });
        let (tool, _) = escalation_tool(PermissionMode::ReadOnly, Some(approver));
        let result = tool
            .execute(
                "c1",
                serde_json::json!({"command": "git push", "sandbox_permissions": "danger-full-access", "justification": "need to push"}),
                CancellationToken::new(),
                &ctx("/base"),
            )
            .await
            .unwrap();
        assert!(output_text(&result).contains("ran=unsandboxed"));
        assert_eq!(
            asked.load(Ordering::SeqCst),
            1,
            "the approver was asked once"
        );
    }

    #[tokio::test]
    async fn sandbox_permissions_non_widening_fails_without_prompting() {
        // workspace-write -> workspace-write is not strictly wider: the
        // approver is never asked, and the call returns the verbatim error.
        let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let approver = Arc::new(CannedApprover {
            outcome: crate::sandbox::EscalationOutcome::AllowedOnce,
            asked: Arc::clone(&asked),
        });
        let (tool, _) = escalation_tool(PermissionMode::WorkspaceWrite, Some(approver));
        let err = tool
            .execute(
                "c1",
                serde_json::json!({"command": "ls", "sandbox_permissions": "workspace-write", "justification": "x"}),
                CancellationToken::new(),
                &ctx("/base"),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not strictly wider"), "{}", err);
        assert_eq!(
            asked.load(Ordering::SeqCst),
            0,
            "non-widening never prompts"
        );
    }

    #[tokio::test]
    async fn sandbox_permissions_without_approver_fails_closed() {
        let (tool, _) = escalation_tool(PermissionMode::ReadOnly, None);
        let err = tool
            .execute(
                "c1",
                serde_json::json!({"command": "ls", "sandbox_permissions": "workspace-write", "justification": "x"}),
                CancellationToken::new(),
                &ctx("/base"),
            )
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("no approval service is composed"),
            "{}",
            err
        );
    }

    #[test]
    fn sandboxed_bash_needs_no_approval_unless_sandbox_permissions() {
        let (tool, _) = escalation_tool(PermissionMode::WorkspaceWrite, None);
        // A plain sandboxed call rides the OS confinement: no gate.
        assert!(!tool.requires_approval(&serde_json::json!({"command": "ls"})));
        // A sandbox_permissions escalation needs the approval round-trip.
        assert!(tool.requires_approval(
            &serde_json::json!({"command": "ls", "sandbox_permissions": "danger-full-access", "justification": "x"})
        ));
    }

    #[test]
    fn without_a_sandbox_every_call_needs_approval() {
        let ops = Arc::new(TaggedOps {
            tag: "t",
            calls: Mutex::new(Vec::new()),
        });
        let tool = BashTool::new(ops, Arc::new(NoopRegistry));
        assert!(
            tool.requires_approval(&serde_json::json!({"command": "ls"})),
            "no OS confinement → the gate is the only barrier"
        );
    }

    #[tokio::test]
    async fn background_summary_honors_tail_lines() {
        let registry = Arc::new(BackgroundRegistry::new());
        let manager = Arc::new(BackgroundManager::new(Arc::clone(&registry)));
        let seen: Arc<Mutex<Vec<pi::types::AgentMessage>>> = Arc::new(Mutex::new(Vec::new()));
        let seen2 = Arc::clone(&seen);
        manager.set_test_steerer(move |m| seen2.lock().unwrap().push(m));
        let tool =
            BashTool::new(Arc::new(EchoOps), registry.clone()).with_manager(Arc::clone(&manager));

        let result = tool
            .execute(
                "c1",
                serde_json::json!({
                    "command": "printf 'a\nb\nc\nd\ne\n'",
                    "run_in_background": true,
                    "tail_lines": 2,
                }),
                CancellationToken::new(),
                &ctx("/tmp"),
            )
            .await
            .unwrap();
        let start = output_text(&result);
        let id = start
            .strip_prefix("Started in background as `")
            .and_then(|t| t.split('`').next())
            .unwrap_or_else(|| panic!("background id in start text: {start}"));

        let summary = test_helpers::wait_for_steered(&seen, 1).await;
        let section = summary
            .split("Recent output:\n")
            .nth(1)
            .unwrap_or_else(|| panic!("shaped tail in summary: {summary}"));
        let lines: Vec<&str> = section
            .lines()
            .take_while(|l| !l.trim().is_empty())
            .collect();
        assert_eq!(lines, vec!["d", "e"], "tail lines: {lines:?}");
        assert!(summary.contains(id), "summary names the task: {summary}");
        let status = registry.status(&pi::TaskId(id.to_string()), 0).unwrap();
        assert!(!status.is_running, "task finished");
    }
}

/// Shared test helpers for the bash module's test suites.
#[cfg(test)]
pub(crate) mod test_helpers {
    use pi::types::AgentMessage;
    use std::sync::Mutex;

    /// Wait until `count` completion summaries reached the recording steerer
    /// and return the most recent one.
    pub async fn wait_for_steered(seen: &Mutex<Vec<AgentMessage>>, count: usize) -> String {
        for _ in 0..100 {
            {
                let msgs = seen.lock().unwrap();
                if msgs.len() >= count
                    && let Some(summary) = msgs.iter().rev().find_map(|m| match m {
                        AgentMessage::User { content, .. } => {
                            content.iter().find_map(|b| match b {
                                pi::types::ContentBlock::Text { text, .. } => Some(text.clone()),
                                _ => None,
                            })
                        }
                        _ => None,
                    })
                {
                    return summary;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!(
            "completion summary not steered in time; {} messages seen",
            seen.lock().unwrap().len()
        );
    }
}
