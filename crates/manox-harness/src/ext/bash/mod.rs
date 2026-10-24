// Bash tool enhancements — the product-level bash tool built on the core
// `BashOperations` / `BackgroundTaskRegistry` seams.
//
// The wrapper keeps the core bash tool untouched: it widens the parameter
// schema (cwd, run_in_background, head/tail line filters) and dispatches
// either to the injected execution backend or to the background registry.

pub mod background;
pub mod orchestration;
pub mod persistent;

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use crate::core::BackgroundTaskRegistry;
use crate::core::env::CommandResult;
use crate::sandbox::{
    AccessMode, EscalationApprover, EscalationRequest, NO_GRANT, NetworkAccess, approve_escalation,
    validate_escalation_args,
};

use crate::core::tool::{AgentTool, AgentToolResult, ToolContext, ToolError};
use crate::core::tools::bash::{BashExecRequest, BashOperations};
use crate::core::tools::truncate::{self, TruncateConfig};
use orchestration::{BackgroundManager, OutputShape};
use serde_json::Value as JsonValue;
use tokio_util::sync::CancellationToken;

/// Default max bytes for output.
const DEFAULT_MAX_BYTES: usize = 128 * 1024;
/// Default max lines for output.
const DEFAULT_MAX_LINES: usize = 2000;
/// Wall-clock limit before a hung command is killed.
const DEFAULT_TIMEOUT_MS: u64 = 120_000;

/// The shared tail of both description variants: background execution,
/// output shaping, and the sandbox-escalation contract.
macro_rules! description_tail {
    () => {
        "`run_in_background: true` starts the command in a fresh shell (no \
         persistent state) at the session cwd — a completion summary with the \
         output tail arrives automatically and **wakes the idle session** so you \
         can act on it immediately. Never use `sleep` or poll loops to wait for \
         a background task — it will wake you when done; to block on one, call \
         `BashOutput` with `wait: true`. Use `BashOutput` \
         (shell_id) for the full output; `TaskStop` stops the task. Bash \
         commands within one batch run serially. \
         \
         Use `head_lines`/`tail_lines` to keep a selection of the output instead of \
         piping through `head`/`tail`. Commands run under a file sandbox; a blocked file operation \
         is reported as `[sandbox: file access denied under <mode> mode]` — a policy denial, not a \
         bug. When a command is denied and full access would let it succeed, retry the exact same \
         command once with `require_escalated: true` plus a \
         one-sentence `justification`; the approval prompt asks the user. Never escalate \
         speculatively — ground the request in a real denial."
    };
}

/// Description where foreground commands run one-shot through the task
/// registry (a seatbelt backend plus a background manager): no shell state
/// carries across calls, and a timeout promotes instead of killing.
const DESCRIPTION_ONE_SHOT: &str = concat!(
    "Execute a shell command. Each call runs in a fresh shell at its `cwd`; \
     no shell state carries across calls. \
     \
     **Concurrency model**: foreground calls block this turn; at their timeout \
     a still-running command moves to the background (the result names its task \
     id) instead of being killed. ",
    description_tail!()
);

/// Description where foreground commands run in the persistent shell (or
/// without a background manager): state persists, and a timeout kills.
const DESCRIPTION_PERSISTENT: &str = concat!(
    "Execute a shell command. Exported vars and functions persist across calls; \
     each call runs at its `cwd`. \
     \
     **Concurrency model**: foreground calls block this turn and are killed at \
     their timeout. ",
    description_tail!()
);

/// A foreground command's raw output, kept per stream and decoded once at
/// the end so a multi-byte character split across reads stays whole.
#[derive(Default)]
struct CapturedOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl CapturedOutput {
    fn take(
        &mut self,
        chunk: background::OutputChunk,
        on_data: Option<crate::core::tools::bash::BashDataCallback<'_>>,
    ) {
        if let Some(on_data) = on_data {
            on_data(&chunk.bytes);
        }
        match chunk.stream {
            background::OutputStream::Stdout => self.stdout.extend_from_slice(&chunk.bytes),
            background::OutputStream::Stderr => self.stderr.extend_from_slice(&chunk.bytes),
        }
    }

    fn into_result(self, exit_code: i32) -> CommandResult {
        CommandResult {
            stdout: String::from_utf8_lossy(&self.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&self.stderr).into_owned(),
            exit_code,
        }
    }
}

/// A registry task a foreground call owns. Unless the call finishes it or
/// releases it to the background, dropping the owner (cancellation, or the
/// call's future being dropped mid-wait) kills the process group and drops
/// the record — a foreground command never outlives its call unowned.
struct ForegroundTask {
    registry: Arc<background::BackgroundRegistry>,
    id: crate::core::TaskId,
    owned: bool,
}

impl ForegroundTask {
    fn new(registry: Arc<background::BackgroundRegistry>, id: crate::core::TaskId) -> Self {
        ForegroundTask {
            registry,
            id,
            owned: true,
        }
    }

    /// The command finished inside the call: its record goes with it.
    fn finish(&mut self) {
        self.owned = false;
        self.registry.forget(&self.id);
    }

    /// The command now belongs to the background manager.
    fn release(&mut self) {
        self.owned = false;
    }
}

impl Drop for ForegroundTask {
    fn drop(&mut self) {
        if self.owned {
            let _ = self.registry.kill_sync(&self.id);
            self.registry.forget(&self.id);
        }
    }
}

/// The bash tool with the manox product surface: array-form `cwd`,
/// background execution via the registry, head/tail line filters, and a
/// `require_escalated` escalation slot. The per-call effective access mode
/// (sandbox / full-access) selects the backend: `full-access` runs through
/// `unsandboxed_operations` (no confinement); `sandbox` rides the sandboxed
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
    /// Backend used when the effective mode is `full-access`
    /// (host-forced, or an approved escalation grant). Absent, such calls
    /// fall back to the default backend.
    unsandboxed_operations: Option<Arc<dyn BashOperations>>,
    /// Host-injected session-mode resolver. Returns the standing mode the
    /// call runs under absent an escalation grant.
    mode_resolver: Option<Arc<dyn Fn() -> AccessMode + Send + Sync>>,
    /// Host-injected network-state resolver, read per call for the denial
    /// classification (a closed network re-words the marker) and mirrored to
    /// the sandboxed backend through its own resolver. Defaults to closed.
    network_resolver: Option<Arc<dyn Fn() -> NetworkAccess + Send + Sync>>,
    /// Host-injected approval channel for a `require_escalated` escalation.
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
            network_resolver: None,
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
    /// `full-access` run through this backend instead of the
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
        resolver: Arc<dyn Fn() -> AccessMode + Send + Sync>,
    ) -> Self {
        self.mode_resolver = Some(resolver);
        self
    }

    /// Bind the network-state resolver (the session's `networkAccess`, read
    /// per call). Hosts wire this to their gate's network cell alongside the
    /// sandboxed backend's own resolver.
    pub fn with_network_resolver(
        mut self,
        resolver: Arc<dyn Fn() -> NetworkAccess + Send + Sync>,
    ) -> Self {
        self.network_resolver = Some(resolver);
        self
    }

    /// Bind the escalation approval channel. Required when a sandbox backend
    /// is mounted and the model may pass `require_escalated`; absent,
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
        if self.promotes_on_timeout() {
            DESCRIPTION_ONE_SHOT
        } else {
            DESCRIPTION_PERSISTENT
        }
    }

    /// `false`: bash mutates (it is not a read-only tool). Note this is the
    /// *tool* classification, not the *mode* effect — under sandbox the
    /// seatbelt still lets bash run, it just denies file writes outside the
    /// writable roots; the two notions are distinct.
    fn is_read_only(&self) -> bool {
        false
    }

    fn requires_approval(&self, params: &JsonValue) -> bool {
        // No OS sandbox (Linux/Windows) → no confinement to stand in for
        // the gate: every call needs approval. With a seatbelt, confined
        // calls ride the OS confinement (no approval); only a
        // `require_escalated` escalation needs the approval round-trip —
        // the escalation itself is the sensitive act.
        //
        // On macOS+seatbelt this returning `true` for an escalation is only a
        // signal that `EscalationGrant::resolve` will run the round-trip
        // *inside* the tool — the host gate's `auto_allow` still delegates
        // bash ungated, so there is no double-prompt (no host gate round-trip).
        if !self.sandbox_available {
            return true;
        }
        params
            .get("require_escalated")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    /// The persistent shell is stateful: a batch of parallel bash calls
    /// would interleave their mutations.
    fn execution_mode(&self) -> crate::core::tool::ExecutionMode {
        crate::core::tool::ExecutionMode::Sequential
    }

    fn parameters_schema(&self) -> JsonValue {
        let mut properties = serde_json::Map::new();
        properties.insert(
            "command".into(),
            serde_json::json!({"type": "string", "description": "The command to execute"}),
        );
        properties.insert(
            "timeout".into(),
            serde_json::json!({"type": "integer", "description": if self.promotes_on_timeout() {
                "Timeout in milliseconds (default: 120000). On expiry the command moves to the background instead of being killed."
            } else {
                "Timeout in milliseconds (default: 120000). On expiry the command is killed."
            }}),
        );
        properties.insert(
            "cwd".into(),
            serde_json::json!({
                "type": "array",
                "items": { "type": "string" },
                "minItems": 1,
                "description": crate::core::tools::path_utils::CWD_SCHEMA_DOC
            }),
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
                "require_escalated".into(),
                serde_json::json!({
                    "type": "boolean",
                    "description": "Set true to retry this exact command with full access (no sandbox). Only valid as a one-shot retry of a command the sandbox just denied; requires justification and user approval."
                }),
            );
            properties.insert(
                "justification".into(),
                serde_json::json!({"type": "string", "description": "Required with require_escalated: one sentence for the user explaining why this exact command needs full access."}),
            );
        }
        serde_json::json!({
            "type": "object",
            "properties": properties,
            "required": ["command", "cwd"]
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
        progress: &dyn crate::core::tool::ToolProgress,
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
        on_data: Option<crate::core::tools::bash::BashDataCallback<'_>>,
    ) -> Result<AgentToolResult, ToolError> {
        let command = params["command"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("command is required".into()))?;
        let timeout_ms = params["timeout"].as_u64().unwrap_or(DEFAULT_TIMEOUT_MS);
        // Sticky-cwd resolution: `cwd` is `[anchor, ...route]` — a non-empty
        // anchor re-anchors the session's default directory, the route
        // locates this call only. The persistent shell re-pins to the
        // resolved directory before each exec, so a `cd` inside a command
        // never leaks into the session's working directory.
        let run_cwd = crate::core::tools::path_utils::resolve_effective_cwd(ctx, params.get("cwd"))
            .map_err(ToolError::InvalidArguments)?;
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
            // as foreground calls: full-access → bare spawn, else
            // sandboxed. Without a seatbelt there is no wrapper either way.
            let sandboxed = self.sandbox_available && effective != AccessMode::FullAccess;
            let shape = OutputShape {
                head_lines,
                tail_lines,
            };
            let id = match &self.manager {
                Some(manager) if sandboxed => manager
                    .spawn_sandboxed(&command, &run_cwd, shape)
                    .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?,
                Some(manager) => manager
                    .spawn(&command, &run_cwd, shape)
                    .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?,
                None => self
                    .registry
                    .spawn(&command, &run_cwd)
                    .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?,
            };
            return Ok(AgentToolResult::text(format!(
                "Started in background as `{id}`. A completion summary with the output tail will arrive automatically. Use `BashOutput` (shell_id) for the full output; `TaskStop` stops the task."
            )));
        }

        // Foreground with a manager and a one-shot backend: run the command
        // as a real registry task so an expired timeout can hand it to the
        // background instead of killing it. The persistent-shell platforms
        // (no seatbelt backend) keep the exec path — their foreground
        // commands hold shell state a fresh `sh -c` cannot carry — and the
        // harness-standalone shape (no manager) keeps the deadline kill.
        if let Some(manager) = self.manager.as_ref().filter(|_| self.promotes_on_timeout()) {
            return self
                .run_foreground_tracked(
                    manager, &command, &run_cwd, timeout_ms, head_lines, tail_lines, effective,
                    signal, on_data, ctx,
                )
                .await;
        }

        let backend = if effective == AccessMode::FullAccess {
            self.unsandboxed_operations
                .as_deref()
                .unwrap_or(&*self.operations)
        } else {
            &*self.operations
        };
        let result = backend
            .exec(BashExecRequest {
                command: &command,
                cwd: Some(&run_cwd),
                timeout: Some(Duration::from_millis(timeout_ms)),
                signal,
                on_data,
            })
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("{e}")))?;

        let mut output = assemble_output(result, head_lines, tail_lines);

        // Per-session-once nudges: suggest native tools when the model reaches
        // for raw grep/rg/find/ls, suggest background for long waits, and
        // point at `cwd` when the command relies on a bare `cd`.
        if !run_in_background {
            output.push_str(&nudge_tool_preference(&command, ctx));
            output.push_str(&nudge_sleep_loop(&command, ctx));
            output.push_str(&nudge_cd_preference(&command, ctx));
        }

        Ok(AgentToolResult::text(output))
    }

    /// Foreground execution as a registry task the call owns: the result is
    /// assembled from the process's own stdout/stderr (complete, split, and
    /// decoded once at the end, exactly like the backend path), streamed to
    /// `on_data` chunk by chunk as it arrives. At the timeout a still-running
    /// command is released to the background instead of being killed —
    /// `adopt` hands it the full background-task lifecycle — and the result
    /// carries its id plus the output so far. A call that ends any other way
    /// (finished, cancelled, or dropped) takes its process with it.
    #[allow(clippy::too_many_arguments)] // one distinct session input per parameter
    async fn run_foreground_tracked(
        &self,
        manager: &Arc<BackgroundManager>,
        command: &str,
        cwd: &std::path::Path,
        timeout_ms: u64,
        head_lines: Option<usize>,
        tail_lines: Option<usize>,
        effective: AccessMode,
        signal: CancellationToken,
        on_data: Option<crate::core::tools::bash::BashDataCallback<'_>>,
        ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        // Same confinement decision as the backend dispatch: an escalated
        // call runs unconfined, everything else through the seatbelt.
        let sandboxed = effective != AccessMode::FullAccess;
        let registry = &manager.registry;
        let (id, mut chunks) = registry
            .spawn_foreground(command, cwd, sandboxed)
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;
        let mut owned = ForegroundTask::new(Arc::clone(registry), id.clone());

        let mut output = CapturedOutput::default();
        let deadline = tokio::time::sleep(Duration::from_millis(timeout_ms.max(1)));
        tokio::pin!(deadline);
        let expired = loop {
            tokio::select! {
                biased;
                () = signal.cancelled() => {
                    return Err(ToolError::ExecutionFailed(
                        "command aborted before completion; the process was stopped".into(),
                    ));
                }
                Some(chunk) = chunks.recv() => output.take(chunk, on_data),
                _ = registry.wait_exit(&id) => break false,
                () = &mut deadline => break true,
            }
        };

        // An exit that lands in the same instant as the deadline reports as
        // finished, not as promoted.
        let exit = registry
            .exit_status(&id)
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;
        if expired && exit.is_none() {
            registry
                .release_foreground(&id)
                .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;
            owned.release();
            while let Ok(chunk) = chunks.try_recv() {
                output.take(chunk, on_data);
            }
            let shape = OutputShape {
                head_lines,
                tail_lines,
            };
            manager
                .adopt(&id, command, shape)
                .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;
            let mut out = format!(
                "Timed out after {}s; moved to background as `{id}`. Use BashOutput (wait: true) to keep waiting; TaskStop to stop.",
                Duration::from_millis(timeout_ms).as_secs(),
            );
            let so_far = assemble_output(output.into_result(0), head_lines, tail_lines);
            let so_far = so_far.trim_end();
            if !so_far.is_empty() {
                out.push_str("\n\nOutput so far:\n");
                out.push_str(so_far);
            }
            out.push_str(&nudge_tool_preference(command, ctx));
            out.push_str(&nudge_sleep_loop(command, ctx));
            out.push_str(&nudge_cd_preference(command, ctx));
            return Ok(AgentToolResult::text(out));
        }

        // Every chunk is forwarded before the exit is recorded, so the queue
        // holds the rest of the output now.
        while let Ok(chunk) = chunks.try_recv() {
            output.take(chunk, on_data);
        }
        owned.finish();
        let signaled = exit == Some(None);
        let code = exit.flatten().unwrap_or(0);
        let mut result = output.into_result(code);
        if sandboxed
            && let Some(note) = crate::sandbox::seatbelt_denial_note(
                &result.stderr,
                code,
                effective,
                self.network_access(),
            )
        {
            result.stderr.push_str(&note);
        }
        let mut out = assemble_output(result, head_lines, tail_lines);
        if signaled {
            out.push_str("\n\n[terminated by signal]");
        }
        out.push_str(&nudge_tool_preference(command, ctx));
        out.push_str(&nudge_sleep_loop(command, ctx));
        out.push_str(&nudge_cd_preference(command, ctx));
        Ok(AgentToolResult::text(out))
    }

    /// Whether a foreground command outlives its timeout as a background
    /// task: it needs a manager to adopt it and a one-shot backend (the
    /// persistent shell's state cannot move into a fresh process).
    fn promotes_on_timeout(&self) -> bool {
        self.manager.is_some() && self.sandbox_available
    }

    /// The standing session mode (absent any per-call grant).
    fn standing_mode(&self) -> AccessMode {
        self.mode_resolver.as_ref().map(|r| r()).unwrap_or_default()
    }

    /// The session's network state, read per call. Defaults to closed — the
    /// sandbox vocabulary's own default — so a host that never wired the
    /// resolver classifies under the stricter face.
    fn network_access(&self) -> NetworkAccess {
        self.network_resolver
            .as_ref()
            .map(|r| r())
            .unwrap_or_default()
    }

    /// The per-call effective mode: an approved escalation grant stamped on
    /// the shared cell, else the standing session mode.
    fn effective_mode(&self) -> AccessMode {
        if let Some(cell) = &self.grant_cell {
            let g = cell.load(Ordering::SeqCst);
            if g != NO_GRANT {
                return AccessMode::from_i64(g);
            }
        }
        self.standing_mode()
    }
}

/// A one-shot sandbox-escalation grant: validates the argument pairing,
/// resolves the wider mode through the host approval channel, stamps it on
/// the shared grant cell for exactly this call, and clears the stamp on drop
/// so the next call reverts to the standing mode.
///
/// The grant is per-call, not per-execution — if `resolve` succeeds but a
/// later step panics before the backend runs, the `Drop` clears the stamp and
/// the user-approved grant never executes — a grant is
/// a same-turn retry signal, not a durable promise: the model re-issues the
/// call and re-escalates. Acceptable for the same-turn retry contract.
struct EscalationGrant<'a> {
    cell: Option<&'a Arc<AtomicI64>>,
}

impl<'a> EscalationGrant<'a> {
    async fn resolve(
        tool: &'a BashTool,
        tool_call_id: &str,
        signal: &CancellationToken,
        params: &JsonValue,
        standing: AccessMode,
    ) -> Result<Self, ToolError> {
        let escalate = params
            .get("require_escalated")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let just = params.get("justification").and_then(|v| v.as_str());
        validate_escalation_args(escalate, just).map_err(ToolError::InvalidArguments)?;
        let guard = Self {
            cell: tool.grant_cell.as_ref(),
        };
        if escalate {
            let approver = tool.escalation_approver.as_ref().ok_or_else(|| {
                ToolError::Other(
                    "sandbox escalation requires approval, but no approval service is composed"
                        .into(),
                )
            })?;
            let grant = approve_escalation(
                EscalationRequest {
                    requested_mode: AccessMode::FullAccess,
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

/// Per-session-once nudge: when the command is a bare grep/rg/find/ls call,
/// suggest using the native tool instead (no sandbox, no approval).
fn nudge_tool_preference(command: &str, ctx: &dyn ToolContext) -> String {
    let trimmed = command.trim();
    // Check for bare grep/rg/find/ls commands (not piped, not part of a chain).
    let is_native_candidate = trimmed.starts_with("grep ")
        || trimmed.starts_with("rg ")
        || trimmed.starts_with("find ")
        || trimmed.starts_with("ls ")
        || trimmed == "grep"
        || trimmed == "rg"
        || trimmed == "find"
        || trimmed == "ls";
    if !is_native_candidate {
        return String::new();
    }
    let nf = &ctx.tool_state().nudge_flags;
    if crate::core::tool::nudge::mark(nf, crate::core::tool::nudge::GREP_TOOL_PREF) {
        return String::new(); // already shown this session
    }
    // Determine which native tool to suggest.
    let native = if trimmed.starts_with("grep") || trimmed.starts_with("rg") {
        "Grep"
    } else if trimmed.starts_with("find") {
        "Glob"
    } else {
        "Ls"
    };
    format!(
        "\n\n[tip: prefer the `{native}` tool over raw `{}` — no sandbox, no approval]",
        trimmed.split_whitespace().next().unwrap_or("cmd")
    )
}

/// Per-session-once nudge: a bare `cd` command (or one starting with `cd`)
/// suggests the `cwd` array instead — the shell's `cd` does not move the
/// session's working directory, so the model must carry locations in `cwd`.
fn nudge_cd_preference(command: &str, ctx: &dyn ToolContext) -> String {
    let trimmed = command.trim();
    if !(trimmed.starts_with("cd ") || trimmed == "cd") {
        return String::new();
    }
    let nf = &ctx.tool_state().nudge_flags;
    if crate::core::tool::nudge::mark(nf, crate::core::tool::nudge::CD_PREFERENCE) {
        return String::new(); // already shown this session
    }
    "\n\n[harness] tip: a `cd` inside a Bash command does not move this session's working \
     directory. Pass the location with `cwd` instead — array form [anchor, ...route]: the \
     first element re-anchors the session (\"\" keeps it), the rest locate just this call. \
     Example: {\"command\": \"cargo test\", \"cwd\": [\"~/projects/dspo/manox\", \
     \"crates/manox-harness\"]}."
        .to_string()
}

/// Per-session-once nudges for long-running foreground patterns.
fn nudge_sleep_loop(command: &str, ctx: &dyn ToolContext) -> String {
    let trimmed = command.trim();
    let nf = &ctx.tool_state().nudge_flags;

    // Pure sleep N (N >= 10): suggest background.
    if let Some(rest) = trimmed.strip_prefix("sleep ") {
        let secs: u64 = rest
            .split_whitespace()
            .next()
            .unwrap_or("0")
            .parse()
            .unwrap_or(0);
        if secs >= 10
            && !crate::core::tool::nudge::mark(nf, crate::core::tool::nudge::SLEEP_BACKGROUND)
        {
            return "\n\n[tip: long wait detected — use `run_in_background: true`; the completion \
                    summary wakes you automatically, or block on it with `BashOutput` (`wait: true`)]"
                .to_string();
        }
    }

    // Poll loop: while/for/until + sleep.
    if (trimmed.starts_with("while ")
        || trimmed.starts_with("for ")
        || trimmed.starts_with("until "))
        && trimmed.contains("sleep")
        && !crate::core::tool::nudge::mark(nf, crate::core::tool::nudge::POLL_LOOP)
    {
        return "\n\n[tip: poll loop detected — use `run_in_background: true` and `BashOutput` \
                (`wait: true`) to block on it, or let the completion wake-up fire]"
            .to_string();
    }

    // gh run watch / gh pr checks --watch: suggest background.
    if (trimmed.contains("gh run watch") || trimmed.contains("gh pr checks --watch"))
        && !crate::core::tool::nudge::mark(nf, crate::core::tool::nudge::GH_WATCH)
    {
        return "\n\n[tip: CI wait detected — use `run_in_background: true` and `BashOutput` \
                (`wait: true`) to block until the CI finishes, or let the completion wake-up fire]"
            .to_string();
    }

    String::new()
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
    use crate::core::tools::bash::BashOperations;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    use super::background::BackgroundRegistry;

    struct EchoOps;
    #[async_trait::async_trait]
    impl BashOperations for EchoOps {
        async fn exec(
            &self,
            request: BashExecRequest<'_>,
        ) -> Result<CommandResult, crate::core::env::ExecutionError> {
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
        fn spawn(
            &self,
            _command: &str,
            _cwd: &Path,
        ) -> Result<crate::core::TaskId, crate::core::TaskError> {
            Ok(crate::core::TaskId("bg_test".into()))
        }
        async fn poll(
            &self,
            _id: &crate::core::TaskId,
        ) -> Result<crate::core::PollResult, crate::core::TaskError> {
            Ok(crate::core::PollResult {
                new_output: String::new(),
                is_running: false,
                exit_code: Some(Some(0)),
                total_bytes: 0,
            })
        }
        async fn kill(&self, _id: &crate::core::TaskId) -> Result<(), crate::core::TaskError> {
            Ok(())
        }
        async fn collect_exit(
            &self,
            _id: &crate::core::TaskId,
        ) -> Result<(), crate::core::TaskError> {
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
        ) -> Result<CommandResult, crate::core::env::ExecutionError> {
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
    impl crate::core::env::ExecutionEnv for MockEnv {
        fn cwd(&self) -> &Path {
            &self.cwd
        }
        fn join_path(&self, parts: &[&str]) -> PathBuf {
            parts.iter().collect()
        }
        async fn absolute_path(&self, path: &Path) -> Result<PathBuf, crate::core::env::FileError> {
            Ok(path.to_path_buf())
        }
        async fn read_file(
            &self,
            _path: &Path,
            _offset: Option<usize>,
            _limit: Option<usize>,
        ) -> Result<String, crate::core::env::FileError> {
            Ok(String::new())
        }
        async fn write_file(
            &self,
            _path: &Path,
            _content: &str,
        ) -> Result<(), crate::core::env::FileError> {
            Ok(())
        }
        async fn exists(&self, _path: &Path) -> Result<bool, crate::core::env::FileError> {
            Ok(true)
        }
        async fn file_info(
            &self,
            _path: &Path,
        ) -> Result<crate::core::env::FileInfo, crate::core::env::FileError> {
            Ok(crate::core::env::FileInfo {
                path: _path.to_path_buf(),
                is_dir: false,
                size: 0,
            })
        }
        async fn list_dir(
            &self,
            _path: &Path,
        ) -> Result<Vec<crate::core::env::FileInfo>, crate::core::env::FileError> {
            Ok(vec![])
        }
        async fn create_dir(&self, _path: &Path) -> Result<(), crate::core::env::FileError> {
            Ok(())
        }
        async fn remove(&self, _path: &Path) -> Result<(), crate::core::env::FileError> {
            Ok(())
        }
        async fn exec(
            &self,
            _command: &str,
            _timeout: Duration,
            _signal: CancellationToken,
        ) -> Result<CommandResult, crate::core::env::ExecutionError> {
            Ok(CommandResult {
                stdout: String::new(),
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
        ) -> Result<CommandResult, crate::core::env::ExecutionError> {
            Ok(CommandResult {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
            })
        }
    }

    fn ctx_at(dir: &Path) -> crate::core::tool::LocalToolContext {
        crate::core::tool::LocalToolContext::new(
            Arc::new(MockEnv {
                cwd: dir.to_path_buf(),
            }),
            dir.to_path_buf(),
            Arc::new(crate::core::tool::ToolState::new()),
        )
    }

    /// A context rooted at a fresh real directory — sticky-cwd resolution
    /// rejects a working directory that does not exist. The tempdir is kept
    /// for the test's lifetime.
    fn ctx() -> (crate::core::tool::LocalToolContext, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        (ctx_at(dir.path()), dir)
    }

    #[tokio::test]
    async fn dispatches_cwd_and_assembles_output() {
        let ops = Arc::new(RecordingOps {
            calls: Mutex::new(Vec::new()),
        });
        let tool = BashTool::new(ops.clone(), Arc::new(NoopRegistry));
        let (ctx, _dir) = ctx();
        let work = tempfile::tempdir().expect("tempdir");
        let result = tool
            .execute(
                "c1",
                serde_json::json!({
                    "command": "echo hi",
                    "cwd": [work.path().to_string_lossy()],
                }),
                CancellationToken::new(),
                &ctx,
            )
            .await
            .unwrap();
        let calls = ops.calls.lock().unwrap();
        assert_eq!(
            calls[0].1,
            Some(work.path().to_path_buf()),
            "cwd override reaches the backend"
        );
        assert!(!result.is_error);
        // A single-element cwd re-anchors the session: the sticky advances to
        // the anchor directory.
        assert_eq!(
            ctx.tool_state().sticky_cwd.lock().unwrap().as_deref(),
            Some(work.path()),
            "sticky cwd follows the anchor"
        );
    }

    #[tokio::test]
    async fn run_in_background_returns_a_task_id() {
        let tool = BashTool::new(Arc::new(EchoOps), Arc::new(NoopRegistry));
        let (ctx, _dir) = ctx();
        let result = tool
            .execute(
                "c1",
                serde_json::json!({"command": "sleep 5", "cwd": [""], "run_in_background": true}),
                CancellationToken::new(),
                &ctx,
            )
            .await
            .unwrap();
        let text = match &result.content[0] {
            crate::core::types::ContentBlock::Text { text, .. } => text.clone(),
            _ => panic!("expected text"),
        };
        assert!(
            text.contains("bg_test"),
            "returns the background id: {text}"
        );
    }

    // ── foreground timeout promotion ─────────────────────────────────────

    /// A manager + a one-shot backend put foreground commands through the
    /// registry so a timeout promotes instead of killing.
    fn tracked_tool() -> (
        BashTool,
        Arc<orchestration::BackgroundManager>,
        Arc<background::BackgroundRegistry>,
    ) {
        let registry = Arc::new(background::BackgroundRegistry::new());
        let manager = orchestration::BackgroundManager::new(Arc::clone(&registry));
        let tool = BashTool::new(Arc::new(EchoOps), Arc::new(NoopRegistry))
            .with_manager(Arc::clone(&manager))
            .with_sandbox_available(true);
        (tool, manager, registry)
    }

    fn result_text(result: crate::core::tool::AgentToolResult) -> String {
        match result.content.into_iter().next() {
            Some(crate::core::types::ContentBlock::Text { text, .. }) => text,
            other => panic!("expected text content, got {other:?}"),
        }
    }

    /// Extract the `bg_N` id a promotion message names.
    fn promoted_id(text: &str) -> crate::core::TaskId {
        let start = text.find('`').expect("promotion names its id") + 1;
        let end = text[start..].find('`').expect("id closes") + start;
        crate::core::TaskId(text[start..end].to_string())
    }

    #[tokio::test]
    async fn foreground_timeout_moves_the_task_to_the_background() {
        let (tool, _manager, registry) = tracked_tool();
        let (ctx, _dir) = ctx();
        let result = tool
            .execute(
                "c1",
                serde_json::json!({
                    "command": "echo pre; sleep 30",
                    "cwd": [""],
                    "timeout": 1500,
                }),
                CancellationToken::new(),
                &ctx,
            )
            .await
            .unwrap();
        let text = result_text(result);
        assert!(
            text.contains("Timed out after 1s; moved to background as `bg_"),
            "promotion message: {text}"
        );
        assert!(
            text.contains("BashOutput (wait: true)"),
            "follow-up guidance: {text}"
        );
        assert!(
            text.contains("Output so far") && text.contains("pre"),
            "carries the output produced so far: {text}"
        );
        let id = promoted_id(&text);
        assert!(
            registry.status(&id, 0).unwrap().is_running,
            "the task must survive the expired timeout"
        );
        registry.kill(&id).await.unwrap();
    }

    #[tokio::test]
    async fn foreground_finished_within_budget_assembles_like_the_backend() {
        let (tool, _manager, _registry) = tracked_tool();
        let (ctx, _dir) = ctx();
        let result = tool
            .execute(
                "c1",
                serde_json::json!({
                    "command": "echo hello",
                    "cwd": [""],
                    "timeout": 10_000,
                }),
                CancellationToken::new(),
                &ctx,
            )
            .await
            .unwrap();
        let text = result_text(result);
        assert!(text.contains("hello"), "output observed: {text}");
        assert!(!text.contains("Timed out"), "finished in budget: {text}");
        assert!(
            !text.contains("exit code"),
            "zero exit stays unannotated: {text}"
        );

        let result = tool
            .execute(
                "c2",
                serde_json::json!({
                    "command": "echo oops >&2; exit 3",
                    "cwd": [""],
                    "timeout": 10_000,
                }),
                CancellationToken::new(),
                &ctx,
            )
            .await
            .unwrap();
        let text = result_text(result);
        assert!(
            text.contains("[stderr]\noops"),
            "stderr keeps its own section: {text}"
        );
        assert!(text.contains("[exit code: 3]"), "exit annotated: {text}");
    }

    /// While the foreground wait runs, ring increments stream to the
    /// caller's progress callback — the host terminal stays live.
    #[tokio::test]
    async fn foreground_wait_streams_increments_to_progress() {
        struct RecordingProgress {
            chunks: Mutex<String>,
        }
        impl crate::core::tool::ToolProgress for RecordingProgress {
            fn emit(&self, partial: serde_json::Value) {
                if let Some(text) = partial.get("output").and_then(|v| v.as_str()) {
                    self.chunks.lock().unwrap().push_str(text);
                }
            }
        }
        let (tool, _manager, _registry) = tracked_tool();
        let (ctx, _dir) = ctx();
        let progress = Arc::new(RecordingProgress {
            chunks: Mutex::new(String::new()),
        });
        let result = tool
            .execute_with_progress(
                "c1",
                serde_json::json!({
                    "command": "echo one; sleep 1; echo two",
                    "cwd": [""],
                    "timeout": 10_000,
                }),
                CancellationToken::new(),
                &ctx,
                progress.as_ref(),
            )
            .await
            .unwrap();
        let text = result_text(result);
        assert!(
            text.contains("one") && text.contains("two"),
            "final output: {text}"
        );
        let streamed = progress.chunks.lock().unwrap().clone();
        assert!(
            streamed.contains("one"),
            "the wait streamed early output to progress: {streamed}"
        );
    }

    /// Degradation: without a manager the foreground path keeps the backend
    /// exec (the harness-standalone shape).
    #[tokio::test]
    async fn foreground_without_a_manager_keeps_the_backend_exec() {
        let ops = Arc::new(RecordingOps {
            calls: Mutex::new(Vec::new()),
        });
        let tool = BashTool::new(ops.clone(), Arc::new(NoopRegistry)).with_sandbox_available(true);
        let (ctx, _dir) = ctx();
        tool.execute(
            "c1",
            serde_json::json!({"command": "echo hi", "cwd": [""]}),
            CancellationToken::new(),
            &ctx,
        )
        .await
        .unwrap();
        assert!(
            !ops.calls.lock().unwrap().is_empty(),
            "the backend exec path ran"
        );
    }

    /// Degradation: a persistent-shell platform (no seatbelt backend) keeps
    /// the backend exec even with a manager mounted — foreground state lives
    /// in the shell there, and a fresh `sh -c` cannot carry it.
    #[tokio::test]
    async fn foreground_on_persistent_shell_platforms_keeps_the_backend_exec() {
        let ops = Arc::new(RecordingOps {
            calls: Mutex::new(Vec::new()),
        });
        let registry = Arc::new(background::BackgroundRegistry::new());
        let manager = orchestration::BackgroundManager::new(Arc::clone(&registry));
        let tool =
            BashTool::new(ops.clone(), Arc::new(NoopRegistry)).with_manager(Arc::clone(&manager));
        let (ctx, _dir) = ctx();
        tool.execute(
            "c1",
            serde_json::json!({"command": "echo hi", "cwd": [""]}),
            CancellationToken::new(),
            &ctx,
        )
        .await
        .unwrap();
        assert!(
            !ops.calls.lock().unwrap().is_empty(),
            "the backend exec path ran"
        );
    }

    async fn run_tracked(tool: &BashTool, params: serde_json::Value) -> String {
        let (ctx, _dir) = ctx();
        result_text(
            tool.execute("c1", params, CancellationToken::new(), &ctx)
                .await
                .unwrap(),
        )
    }

    /// A multi-byte character split across two writes arrives whole.
    #[tokio::test]
    async fn foreground_keeps_a_split_utf8_character_whole() {
        let (tool, _manager, _registry) = tracked_tool();
        let text = run_tracked(
            &tool,
            serde_json::json!({
                "command": r"printf '\344\270'; sleep 0.3; printf '\255\346\226\207\n'",
                "cwd": [""],
                "timeout": 10_000,
            }),
        )
        .await;
        assert!(
            text.contains("中文"),
            "split character kept whole: {text:?}"
        );
    }

    /// Output far larger than the background ring keeps its head: the
    /// result is built from the process's own stream, not from the ring.
    #[tokio::test]
    async fn foreground_fast_output_keeps_its_head() {
        let (tool, _manager, _registry) = tracked_tool();
        let text = run_tracked(
            &tool,
            serde_json::json!({
                "command": "seq 1 1000000",
                "cwd": [""],
                "timeout": 60_000,
                "head_lines": 3,
            }),
        )
        .await;
        assert!(
            text.starts_with("1\n2\n3"),
            "head kept: {:?}",
            &text[..text.len().min(80)]
        );
    }

    /// The output carried by a promotion is bounded like any foreground
    /// result, and a later `BashOutput` continues exactly where it stops.
    #[tokio::test]
    async fn promotion_bounds_the_output_so_far_and_hands_off_the_cursor() {
        let (tool, _manager, registry) = tracked_tool();
        let text = run_tracked(
            &tool,
            serde_json::json!({
                "command": "seq 1 300000; sleep 1; echo after-promotion; sleep 30",
                "cwd": [""],
                "timeout": 500,
            }),
        )
        .await;
        assert!(
            text.len() <= DEFAULT_MAX_BYTES + 4096,
            "promotion output bounded: {} bytes",
            text.len()
        );
        assert!(text.contains("[output truncated:"), "truncation marked");
        let id = promoted_id(&text);
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let poll = registry.poll(&id).await.unwrap();
        assert_eq!(
            poll.new_output, "after-promotion\n",
            "the next read starts where the promotion's output ended"
        );
        registry.kill(&id).await.unwrap();
    }

    /// An escalated foreground command runs through the registry's
    /// unconfined builder (the host's own shell and env), not a bare `sh`.
    #[tokio::test]
    async fn escalated_foreground_uses_the_unconfined_builder() {
        let build: background::SandboxCommandBuilder = Arc::new(|command, cwd| {
            let mut cmd = tokio::process::Command::new("bash");
            cmd.arg("-c")
                .arg(command)
                .current_dir(cwd)
                .env("UNCONFINED_MARK", "host-env");
            cmd
        });
        let registry = Arc::new(background::BackgroundRegistry::new().with_unconfined(build));
        let manager = orchestration::BackgroundManager::new(Arc::clone(&registry));
        let tool = BashTool::new(Arc::new(EchoOps), Arc::new(NoopRegistry))
            .with_manager(manager)
            .with_sandbox_available(true)
            .with_mode_resolver(Arc::new(|| AccessMode::FullAccess));
        let text = run_tracked(
            &tool,
            serde_json::json!({
                "command": "echo \"mark=${UNCONFINED_MARK:-unset}\"; cat <(echo procsub)",
                "cwd": [""],
                "timeout": 10_000,
            }),
        )
        .await;
        assert!(text.contains("mark=host-env"), "host env applied: {text}");
        assert!(text.contains("procsub"), "bash, not sh: {text}");
    }

    /// A foreground call that ends without finishing — its future dropped
    /// mid-wait — takes its process with it.
    #[tokio::test]
    async fn dropped_foreground_call_kills_its_process() {
        let (tool, _manager, _registry) = tracked_tool();
        let (ctx, _dir) = ctx();
        let marker = tempfile::NamedTempFile::new().unwrap().into_temp_path();
        std::fs::remove_file(&marker).unwrap();
        let command = format!("sleep 1; touch '{}'", marker.display());
        let call = tool.execute(
            "c1",
            serde_json::json!({"command": command, "cwd": [""], "timeout": 60_000}),
            CancellationToken::new(),
            &ctx,
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(200), call)
                .await
                .is_err(),
            "the call is dropped mid-wait"
        );
        tokio::time::sleep(Duration::from_millis(1800)).await;
        assert!(!marker.exists(), "the dropped call's process was killed");
    }

    /// A cancelled foreground call reports the abort and stops its process.
    #[tokio::test]
    async fn cancelled_foreground_call_stops_its_process() {
        let (tool, _manager, _registry) = tracked_tool();
        let (ctx, _dir) = ctx();
        let marker = tempfile::NamedTempFile::new().unwrap().into_temp_path();
        std::fs::remove_file(&marker).unwrap();
        let command = format!("sleep 1; touch '{}'", marker.display());
        let signal = CancellationToken::new();
        let canceller = signal.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            canceller.cancel();
        });
        let err = tool
            .execute(
                "c1",
                serde_json::json!({"command": command, "cwd": [""], "timeout": 60_000}),
                signal,
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("aborted"), "got: {err}");
        tokio::time::sleep(Duration::from_millis(1800)).await;
        assert!(!marker.exists(), "the cancelled call's process was stopped");
    }

    /// The description and the `timeout` schema promise promotion only
    /// where it happens.
    #[test]
    fn descriptions_promise_promotion_only_where_it_happens() {
        let (promoting, _manager, _registry) = tracked_tool();
        assert!(promoting.description().contains("moves to the background"));
        let timeout = &promoting.parameters_schema()["properties"]["timeout"]["description"];
        assert!(
            timeout
                .as_str()
                .unwrap()
                .contains("moves to the background")
        );

        let persistent = BashTool::new(Arc::new(EchoOps), Arc::new(NoopRegistry));
        assert!(!persistent.description().contains("moves to the background"));
        assert!(persistent.description().contains("persist across calls"));
        let timeout = &persistent.parameters_schema()["properties"]["timeout"]["description"];
        assert!(timeout.as_str().unwrap().contains("is killed"));
    }

    /// A `cd` inside a command moves only the shell's directory for that
    /// command — the session's sticky cwd never follows it.
    #[tokio::test]
    async fn cd_inside_a_command_does_not_move_the_sticky_cwd() {
        let ops = Arc::new(RecordingOps {
            calls: Mutex::new(Vec::new()),
        });
        let tool = BashTool::new(ops, Arc::new(NoopRegistry));
        let (ctx, dir) = ctx();
        // Anchor once so the sticky holds a real value to protect.
        tool.execute(
            "c1",
            serde_json::json!({
                "command": "echo start",
                "cwd": [dir.path().to_string_lossy()],
            }),
            CancellationToken::new(),
            &ctx,
        )
        .await
        .unwrap();
        tool.execute(
            "c2",
            serde_json::json!({"command": "cd somewhere && pwd", "cwd": [""]}),
            CancellationToken::new(),
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(
            ctx.tool_state().sticky_cwd.lock().unwrap().as_deref(),
            Some(dir.path()),
            "the sticky stays at the anchor; the command's `cd` does not leak"
        );
    }

    /// A `cd` command appends the cwd nudge exactly once per session.
    #[tokio::test]
    async fn cd_command_nudges_once_per_session() {
        let tool = BashTool::new(Arc::new(EchoOps), Arc::new(NoopRegistry));
        let (ctx, _dir) = ctx();
        const TIP: &str = "[harness] tip: a `cd` inside a Bash command";
        let first = tool
            .execute(
                "c1",
                serde_json::json!({"command": "cd sub && pwd", "cwd": [""]}),
                CancellationToken::new(),
                &ctx,
            )
            .await
            .unwrap();
        assert!(output_text(&first).contains(TIP), "first cd nudges");
        let second = tool
            .execute(
                "c2",
                serde_json::json!({"command": "cd other", "cwd": [""]}),
                CancellationToken::new(),
                &ctx,
            )
            .await
            .unwrap();
        assert!(
            !output_text(&second).contains(TIP),
            "the nudge fires once per session"
        );
        let unrelated = tool
            .execute(
                "c3",
                serde_json::json!({"command": "echo hi", "cwd": [""]}),
                CancellationToken::new(),
                &ctx,
            )
            .await
            .unwrap();
        assert!(
            !output_text(&unrelated).contains(TIP),
            "a non-cd command never carries the nudge"
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
        ) -> Result<CommandResult, crate::core::env::ExecutionError> {
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
            crate::core::types::ContentBlock::Text { text, .. } => text.clone(),
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
        let (ctx, _dir) = ctx();
        let result = tool
            .execute(
                "c1",
                serde_json::json!({"command": "echo hi", "cwd": [""]}),
                CancellationToken::new(),
                &ctx,
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
        standing: AccessMode,
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
        let resolver: Arc<dyn Fn() -> AccessMode + Send + Sync> = {
            let sc = Arc::clone(&standing_cell);
            Arc::new(move || AccessMode::from_i64(sc.load(Ordering::SeqCst)))
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
    async fn require_escalated_grants_full_access_for_one_call() {
        // Standing sandbox; an approved escalation to full access routes the
        // call through the unsandboxed backend.
        let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let approver = Arc::new(CannedApprover {
            outcome: crate::sandbox::EscalationOutcome::AllowedOnce,
            asked: Arc::clone(&asked),
        });
        let (tool, _) = escalation_tool(AccessMode::Sandbox, Some(approver));
        let (ctx, _dir) = ctx();
        let result = tool
            .execute(
                "c1",
                serde_json::json!({"command": "git push", "cwd": [""], "require_escalated": true, "justification": "need to push"}),
                CancellationToken::new(),
                &ctx,
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
    async fn require_escalated_same_level_is_silent_no_op() {
        // full-access -> full-access: an idempotent no-op grant. The
        // approver is never asked, the call runs at the standing mode.
        let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let approver = Arc::new(CannedApprover {
            outcome: crate::sandbox::EscalationOutcome::AllowedOnce,
            asked: Arc::clone(&asked),
        });
        let (tool, _) = escalation_tool(AccessMode::FullAccess, Some(approver));
        let (ctx, _dir) = ctx();
        let result = tool
            .execute(
                "c1",
                serde_json::json!({"command": "ls", "cwd": [""], "require_escalated": true, "justification": "x"}),
                CancellationToken::new(),
                &ctx,
            )
            .await
            .unwrap();
        assert!(
            output_text(&result).contains("ran=unsandboxed"),
            "the no-op grant stays at the standing mode"
        );
        assert_eq!(
            asked.load(Ordering::SeqCst),
            0,
            "a same-level request never prompts"
        );
    }

    #[tokio::test]
    async fn require_escalated_without_approver_fails_closed() {
        let (tool, _) = escalation_tool(AccessMode::Sandbox, None);
        let (ctx, _dir) = ctx();
        let err = tool
            .execute(
                "c1",
                serde_json::json!({"command": "ls", "cwd": [""], "require_escalated": true, "justification": "x"}),
                CancellationToken::new(),
                &ctx,
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
    fn sandboxed_bash_needs_no_approval_unless_require_escalated() {
        let (tool, _) = escalation_tool(AccessMode::Sandbox, None);
        // A plain sandboxed call rides the OS confinement: no gate.
        assert!(!tool.requires_approval(&serde_json::json!({"command": "ls"})));
        // A require_escalated escalation needs the approval round-trip.
        assert!(tool.requires_approval(
            &serde_json::json!({"command": "ls", "require_escalated": true, "justification": "x"})
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
        let manager = BackgroundManager::new(Arc::clone(&registry));
        let seen: Arc<Mutex<Vec<crate::core::types::AgentMessage>>> =
            Arc::new(Mutex::new(Vec::new()));
        let seen2 = Arc::clone(&seen);
        manager.set_test_steerer(move |m| seen2.lock().unwrap().push(m));
        let tool =
            BashTool::new(Arc::new(EchoOps), registry.clone()).with_manager(Arc::clone(&manager));

        let (ctx, _dir) = ctx();
        let result = tool
            .execute(
                "c1",
                serde_json::json!({
                    "command": "printf 'a\nb\nc\nd\ne\n'",
                    "cwd": [""],
                    "run_in_background": true,
                    "tail_lines": 2,
                }),
                CancellationToken::new(),
                &ctx,
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
        let status = registry
            .status(&crate::core::TaskId(id.to_string()), 0)
            .unwrap();
        assert!(!status.is_running, "task finished");
    }
}

/// Shared test helpers for the bash module's test suites.
#[cfg(test)]
pub(crate) mod test_helpers {
    use crate::core::types::AgentMessage;
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
                                crate::core::types::ContentBlock::Text { text, .. } => {
                                    Some(text.clone())
                                }
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
