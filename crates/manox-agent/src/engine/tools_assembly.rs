//! Engine tool assembly: the subagent/task tools, browser suites,
//! `build_tools`, the session orchestrator, and the embedder/MCP refresh
//! paths.

use super::*;

/// Infer a capability tag for an agent definition from its declared tool
/// allowlist. The host snapshot carries Read/Grep/Glob/Ls (read),
/// Write/Edit (write), and Bash (exec); `tools: []` means the full
/// snapshot. The tag rides the `ListAgents` catalog so the model knows
/// what each subagent can do before dispatching.
pub(super) fn subagent_capability(def: &manox_harness::ext_point_agent::AgentDef) -> &'static str {
    if def.tools.is_empty() {
        return "write+bash";
    }
    let has_write = def.tools.iter().any(|t| t == "Write" || t == "Edit");
    let has_bash = def.tools.iter().any(|t| t == "Bash");
    match (has_write, has_bash) {
        (true, true) => "write+bash",
        (true, false) => "write",
        (false, true) => "bash",
        (false, false) => "read-only",
    }
}

/// Host wrapper around `manox_harness::bash::BashTool` for the subagent
/// snapshot. A Sailor is already an async primitive, so a background bash
/// inside a subagent session is pointless AND dangerous — the subagent's
/// registry has no manager/seatbelt-wrap, so `run_in_background` would
/// spawn a bare process that bypasses the seatbelt, eludes
/// `TaskStop`/`BashOutput`/UI, and outlives the session (no `Drop` reap).
/// Reject `run_in_background` outright. The escalation fields follow the
/// armed state: with the per-run parent channel attached (the authority's
/// per-run grant cell + approver ride the inner BashTool) a
/// `require_escalated` retry resolves through the parent's approval card;
/// without it the session has no channel and the retry fails closed.
pub(super) struct SubagentBashTool {
    pub(super) inner: Arc<dyn manox_harness::tool::AgentTool>,
    /// The run's escalation channel; `None` on the shared snapshot (the
    /// per-run state is attached by the child authority at spawn).
    pub(super) escalation: Option<Arc<ChildEscalation>>,
}

pub(super) const SUBAGENT_BASH_DESCRIPTION: &str = "Execute a shell command. Each call runs in a \
    fresh one-shot shell at the current cwd (no persistent cwd/vars across calls). Access follows \
    the dispatching session's mode, narrowed by this subagent's declared permission ceiling: \
    under full-access (and with no narrowing declared) the command runs unsandboxed; under \
    sandbox — the dispatching session's or the definition's own — it runs in the same OS \
    confinement (same writable roots, same network switch) as the dispatching session's own \
    Bash. If the sandbox denies a command, retry that exact command once with \
    `require_escalated: true` + `justification` — the request surfaces as an approval card on \
    the parent conversation, attributed to this subagent; an approval widens exactly that one \
    call, and a denial or a spent escalation budget is final. `run_in_background` is not \
    available inside a subagent — the subagent itself is the async primitive; run long commands \
    in the foreground. Use `head_lines`/`tail_lines` to keep a selection of output instead of \
    piping through `head`/`tail`.";

pub(super) const SUBAGENT_BASH_DESCRIPTION_NO_CHANNEL: &str = "Execute a shell command. Each call runs in a \
    fresh one-shot shell at the current cwd (no persistent cwd/vars across calls). Access follows \
    the dispatching session's mode, narrowed by this subagent's declared permission ceiling: \
    under full-access (and with no narrowing declared) the command runs unsandboxed; under \
    sandbox — the dispatching session's or the definition's own — it runs in the same OS \
    confinement (same writable roots, same network switch) as the dispatching session's own \
    Bash. `run_in_background` and `require_escalated` are not available inside a subagent — \
    the subagent itself is the async primitive and it has no approval channel. Use \
    `head_lines`/`tail_lines` to keep a selection of output instead of piping through \
    `head`/`tail`.";

/// Whether the call carries an escalation request.
fn is_escalation_request(params: &serde_json::Value) -> bool {
    params
        .get("require_escalated")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

#[async_trait::async_trait]
impl manox_harness::tool::AgentTool for SubagentBashTool {
    fn name(&self) -> &str {
        "Bash"
    }
    fn description(&self) -> &str {
        if self.escalation.is_some() {
            SUBAGENT_BASH_DESCRIPTION
        } else {
            SUBAGENT_BASH_DESCRIPTION_NO_CHANNEL
        }
    }
    fn is_read_only(&self) -> bool {
        false
    }
    fn requires_approval(&self, params: &serde_json::Value) -> bool {
        // An escalated call on an armed tool parks no boundary gate — the
        // tool runs the approval round trip itself against the parent
        // channel (mirrors the host gate's `auto_allow` for confined bash).
        // Without the channel the call stays approval-bearing and the
        // boundary gate rejects it.
        if self.escalation.is_some() && is_escalation_request(params) {
            return false;
        }
        self.inner.requires_approval(params)
    }
    fn execution_mode(&self) -> manox_harness::tool::ExecutionMode {
        // Delegate: the kernel BashTool declares Sequential (a stateful
        // persistent shell on non-macOS); the wrapper must inherit it so a
        // single Sailor session doesn't interleave parallel bash state.
        self.inner.execution_mode()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        // Strip `run_in_background` (refused by this wrapper) always; strip
        // the escalation fields only when no channel is armed, so the model
        // never proposes a retry it cannot make.
        let mut schema = self.inner.parameters_schema();
        if let Some(props) = schema.get_mut("properties").and_then(|p| p.as_object_mut()) {
            props.remove("run_in_background");
            if self.escalation.is_none() {
                props.remove("require_escalated");
                props.remove("justification");
            }
        }
        schema
    }

    async fn execute(
        &self,
        tool_call_id: &str,
        params: serde_json::Value,
        signal: tokio_util::sync::CancellationToken,
        ctx: &dyn manox_harness::tool::ToolContext,
    ) -> Result<manox_harness::tool::AgentToolResult, manox_harness::tool::ToolError> {
        self.refuse_unavailable(&params)?;
        if let Some(esc) = &self.escalation
            && is_escalation_request(&params)
        {
            // Refuse before the round trip so a spent budget produces the
            // informative text instead of a surprise denial at the channel.
            esc.check_budget()
                .map_err(manox_harness::tool::ToolError::Other)?;
        }
        self.inner.execute(tool_call_id, params, signal, ctx).await
    }

    async fn execute_with_progress(
        &self,
        tool_call_id: &str,
        params: serde_json::Value,
        signal: tokio_util::sync::CancellationToken,
        ctx: &dyn manox_harness::tool::ToolContext,
        progress: &dyn manox_harness::tool::ToolProgress,
    ) -> Result<manox_harness::tool::AgentToolResult, manox_harness::tool::ToolError> {
        self.refuse_unavailable(&params)?;
        if let Some(esc) = &self.escalation
            && is_escalation_request(&params)
        {
            esc.check_budget()
                .map_err(manox_harness::tool::ToolError::Other)?;
        }
        self.inner
            .execute_with_progress(tool_call_id, params, signal, ctx, progress)
            .await
    }
}

impl SubagentBashTool {
    /// Refuse the calls this wrapper can never run: a background spawn
    /// (always), and an escalation request on an unarmed tool (the schema
    /// already strips the fields — this is the non-conforming-model
    /// backstop, and it names the missing channel).
    fn refuse_unavailable(
        &self,
        params: &serde_json::Value,
    ) -> Result<(), manox_harness::tool::ToolError> {
        if params["run_in_background"].as_bool().unwrap_or(false) {
            return Err(manox_harness::tool::ToolError::ExecutionFailed(
                "`run_in_background` is not available inside a subagent session — the subagent \
                 itself is the async primitive. Run the command in the foreground instead."
                    .into(),
            ));
        }
        if self.escalation.is_none() && is_escalation_request(params) {
            return Err(manox_harness::tool::ToolError::ExecutionFailed(
                "`require_escalated` is not available inside a subagent session — this session \
                 has no approval channel, and its access mode already follows the dispatching \
                 session."
                    .into(),
            ));
        }
        Ok(())
    }
}

/// Write/Edit fence for the subagent snapshot: the same containment verdict
/// the Captain's `ApprovalGatedTool` applies, read from the dispatching
/// session's live gate. `full-access` passes through; `sandbox` admits only
/// targets inside the shared `writable_roots` plus the run's granted roots,
/// so a Sailor cannot write where its Bash could not. Wraps the file-locked
/// tool (`fence(FileLocked(tool))`): the fence judges before the lock is
/// taken, so a refusal never holds a path.
///
/// The escalation channel follows the armed state. Armed (the authority's
/// per-run approver + overlay are attached): an out-of-roots target is
/// refused with the sanctioned retry — the model re-issues the call with
/// `require_escalated` + `justification`, the ask parks an approval card on
/// the parent conversation, and an approval records the covered root into
/// the run's overlay (approve once for this run, never re-ask). Unarmed:
/// the marker and the honest fact of a session with no channel.
pub(super) struct SubagentWriteFence {
    pub(super) inner: Arc<dyn HarnessAgentTool>,
    pub(super) mode_resolver: Arc<dyn Fn() -> AccessMode + Send + Sync>,
    pub(super) granted_roots: crate::granted_roots::GrantedRoots,
    /// The run's escalation channel; `None` on the shared snapshot.
    pub(super) escalation: Option<Arc<ChildEscalation>>,
}

/// The unarmed fence's refusal: the shared sandbox marker (the one
/// vocabulary both enforcing families speak) and the honest fact of a
/// session with no escalation channel.
fn subagent_fence_denial() -> manox_harness::tool::ToolError {
    manox_harness::tool::ToolError::Other(format!(
        "{}\n[sandbox: no escalation channel in this session]",
        manox_harness::sandbox::sandbox_denial_marker(AccessMode::Sandbox)
    ))
}

/// The armed fence's refusal for a plain out-of-roots call: the marker plus
/// the sanctioned retry — the one widening road this session has.
fn armed_fence_plain_denial() -> manox_harness::tool::ToolError {
    manox_harness::tool::ToolError::Other(format!(
        "{}\n{}",
        manox_harness::sandbox::sandbox_denial_marker(AccessMode::Sandbox),
        manox_harness::sandbox::escalation_hint_marker("operation")
    ))
}

#[async_trait::async_trait]
impl manox_harness::tool::AgentTool for SubagentWriteFence {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn description(&self) -> &str {
        self.inner.description()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        let mut schema = self.inner.parameters_schema();
        // Advertise the escalation fields on the armed fence, so the model
        // knows a refused call can be retried with `require_escalated` +
        // `justification` and the ask reaches the parent's user.
        if self.escalation.is_some()
            && let Some(props) = schema.get_mut("properties").and_then(|p| p.as_object_mut())
        {
            props.insert(
                "require_escalated".into(),
                serde_json::json!({
                    "type": "boolean",
                    "description": "Set true to retry this exact operation with full access (no sandbox). Only valid as a one-shot retry of an operation the fence just refused; requires justification and user approval on the parent conversation."
                }),
            );
            props.insert(
                "justification".into(),
                serde_json::json!({
                    "type": "string",
                    "description": "Required with require_escalated: one sentence for the user explaining why this exact file operation needs full access."
                }),
            );
        }
        schema
    }
    fn requires_approval(&self, params: &serde_json::Value) -> bool {
        self.inner.requires_approval(params)
    }
    fn is_read_only(&self) -> bool {
        self.inner.is_read_only()
    }
    fn mutates_through_fs_paths(&self) -> bool {
        self.inner.mutates_through_fs_paths()
    }
    fn execution_mode(&self) -> manox_harness::tool::ExecutionMode {
        self.inner.execution_mode()
    }

    async fn execute(
        &self,
        tool_call_id: &str,
        params: serde_json::Value,
        signal: tokio_util::sync::CancellationToken,
        ctx: &dyn manox_harness::tool::ToolContext,
    ) -> Result<manox_harness::tool::AgentToolResult, manox_harness::tool::ToolError> {
        self.verdict(tool_call_id, &params, &signal, ctx).await?;
        self.inner.execute(tool_call_id, params, signal, ctx).await
    }

    async fn execute_with_progress(
        &self,
        tool_call_id: &str,
        params: serde_json::Value,
        signal: tokio_util::sync::CancellationToken,
        ctx: &dyn manox_harness::tool::ToolContext,
        progress: &dyn manox_harness::tool::ToolProgress,
    ) -> Result<manox_harness::tool::AgentToolResult, manox_harness::tool::ToolError> {
        self.verdict(tool_call_id, &params, &signal, ctx).await?;
        self.inner
            .execute_with_progress(tool_call_id, params, signal, ctx, progress)
            .await
    }
}

impl SubagentWriteFence {
    /// The containment verdict, escalating through the parent channel when
    /// armed and the call asks for it.
    async fn verdict(
        &self,
        tool_call_id: &str,
        params: &serde_json::Value,
        signal: &tokio_util::sync::CancellationToken,
        ctx: &dyn manox_harness::tool::ToolContext,
    ) -> Result<(), manox_harness::tool::ToolError> {
        if (self.mode_resolver)() == AccessMode::FullAccess {
            return Ok(());
        }
        match crate::approval::fs_write_verdict(
            self.inner.name(),
            params,
            ctx,
            Some(&self.granted_roots),
            &subagent_fence_denial,
        ) {
            Ok(()) => return Ok(()),
            // A malformed cwd is an argument error, not a policy denial —
            // surface the tool's own instructional text, never an
            // escalation for a call that cannot name its target.
            Err(err @ manox_harness::tool::ToolError::InvalidArguments(_)) => return Err(err),
            Err(_) => {}
        }
        let Some(escalation) = &self.escalation else {
            return Err(subagent_fence_denial());
        };
        if !is_escalation_request(params) {
            return Err(armed_fence_plain_denial());
        }
        let justification = params
            .get("justification")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        manox_harness::sandbox::validate_escalation_args(true, Some(justification))
            .map_err(manox_harness::tool::ToolError::InvalidArguments)?;
        let ask = super::subagent_authority::FsEscalationAsk {
            tool_name: self.inner.name(),
            params,
            justification,
            effective_mode: (self.mode_resolver)(),
        };
        match escalation
            .resolve_fs_escalation(ask, tool_call_id, signal, ctx)
            .await
        {
            // Approved: the covered root joins the run's overlay, so later
            // calls under the same root never re-ask; this call proceeds.
            Ok(root) => {
                self.granted_roots.approve(root);
                Ok(())
            }
            Err(msg) => Err(manox_harness::tool::ToolError::Other(format!(
                "{}\n{msg}",
                manox_harness::sandbox::sandbox_denial_marker(AccessMode::Sandbox)
            ))),
        }
    }
}

/// The shared assembly pieces every child snapshot derives from: the
/// dispatcher's live authority (gate + standing resolver) and the shared
/// backends. The per-dispatch declaration is the only variable — [`build`]
/// turns it into the child's tool set.
///
/// [`build`]: ChildAssembly::build
#[derive(Clone)]
pub(super) struct ChildAssembly {
    pub(super) gate: Arc<ApprovalGate>,
    pub(super) standing_resolver: Arc<dyn Fn() -> AccessMode + Send + Sync>,
    pub(super) shared_ops: Arc<dyn manox_harness::tools::bash::BashOperations>,
    pub(super) unsandboxed_ops: Option<Arc<dyn manox_harness::tools::bash::BashOperations>>,
    pub(super) sandbox_available: bool,
    pub(super) cwd: std::path::PathBuf,
    pub(super) granted_roots: crate::granted_roots::GrantedRoots,
}

impl ChildAssembly {
    /// The child snapshot's tool set for one effective permission
    /// declaration — the per-dispatch shape [`ChildToolFactory`] builds.
    /// The declaration is the intersection of the definition's
    /// `permissions:` frontmatter and any dispatch-time override, already
    /// narrowed by the caller; `None` is the all-inherit default. The
    /// invariant on every axis: **the child's effective permissions are a
    /// subset of the dispatcher's live authority** —
    ///
    /// - `mode: sandbox` pins the mode constant (a read-only agent stays
    ///   read-only even under a `full-access` Captain); `inherit`/
    ///   `full-access` ride the dispatcher's gate live (#965 semantics,
    ///   mid-turn flips included).
    /// - `network: off` pins the confined calls' route closed;
    ///   `inherit`/`on` follow the dispatcher's switch live.
    ///
    /// A pinned axis cannot ride the shared (#965) backend, whose resolvers
    /// read the dispatcher's gate — a sandbox pin would un-pin the moment
    /// the Captain flipped to full-access (the ops layer would
    /// short-circuit to a bare run). Pinned shapes therefore get a
    /// dedicated backend whose resolvers already answer for the child,
    /// standing-mode only (a child has no escalation channel, so the shared
    /// grant cell is not consulted — an escalation stamp belongs to the
    /// call that requested it).
    pub(super) fn build(
        &self,
        effective: Option<&manox_harness::ext_point_agent::AgentPermissions>,
    ) -> Vec<Arc<dyn HarnessAgentTool>> {
        use manox_harness::ext_point_agent::{NetworkPermission, PermissionMode};
        let declared = effective.copied().unwrap_or_default();
        let mode_pinned = declared.mode == PermissionMode::Sandbox;
        let network_pinned_off = declared.network == NetworkPermission::Off;
        // Tool-level resolvers: a pinned axis is constant; an inherited axis
        // reads the dispatcher's live gate.
        let mode_resolver: Arc<dyn Fn() -> AccessMode + Send + Sync> = if mode_pinned {
            Arc::new(|| AccessMode::Sandbox)
        } else {
            Arc::clone(&self.standing_resolver)
        };
        let network_resolver: Arc<dyn Fn() -> manox_harness::sandbox::NetworkAccess + Send + Sync> =
            if network_pinned_off {
                Arc::new(|| manox_harness::sandbox::NetworkAccess::Off)
            } else {
                let gate = Arc::clone(&self.gate);
                Arc::new(move || gate.network_route().access())
            };
        let ops: Arc<dyn manox_harness::tools::bash::BashOperations> =
            if self.sandbox_available && (mode_pinned || network_pinned_off) {
                let ops_mode: Arc<dyn Fn() -> AccessMode + Send + Sync> = if mode_pinned {
                    Arc::new(|| AccessMode::Sandbox)
                } else {
                    Arc::clone(&self.standing_resolver)
                };
                let ops_network: Arc<dyn Fn() -> crate::sandbox::NetworkRoute + Send + Sync> =
                    if network_pinned_off {
                        Arc::new(|| crate::sandbox::NetworkRoute::Disabled)
                    } else {
                        let gate = Arc::clone(&self.gate);
                        Arc::new(move || gate.network_route())
                    };
                Arc::new(crate::sandbox::SandboxedBashOperations::new(
                    &self.cwd,
                    self.granted_roots.clone(),
                    ops_mode,
                    ops_network,
                ))
            } else {
                // No pin (or no OS backend to pin with — the confined face
                // there is the persistent shell either way): the shared
                // #965 backend.
                Arc::clone(&self.shared_ops)
            };
        let bash = subagent_bash_tool(
            ops,
            Arc::new(BackgroundRegistry::new()),
            self.sandbox_available,
            Arc::clone(&mode_resolver),
            network_resolver,
            // A pinned child never selects the unsandboxed backend — it is
            // not installed, so the escape hatch it serves does not exist.
            (!mode_pinned)
                .then(|| self.unsandboxed_ops.clone())
                .flatten(),
        );
        child_snapshot(bash, &mode_resolver, &self.granted_roots)
    }
}

/// The subagent's `BashTool`: the Captain's backends and resolvers without
/// the manager (background is refused upstream) and without the per-call
/// grant cell. Inheritance is live and by pointer — the same `Arc`s — so a
/// mid-turn mode or network switch governs the Sailor's next call exactly as
/// it governs the Captain's.
pub(super) fn subagent_bash_tool(
    operations: Arc<dyn manox_harness::tools::bash::BashOperations>,
    registry: Arc<BackgroundRegistry>,
    sandbox_available: bool,
    mode_resolver: Arc<dyn Fn() -> AccessMode + Send + Sync>,
    network_resolver: Arc<dyn Fn() -> manox_harness::sandbox::NetworkAccess + Send + Sync>,
    unsandboxed_operations: Option<Arc<dyn manox_harness::tools::bash::BashOperations>>,
) -> BashTool {
    let tool = BashTool::new(operations, registry)
        .with_sandbox_available(sandbox_available)
        .with_mode_resolver(mode_resolver)
        .with_network_resolver(network_resolver);
    match unsandboxed_operations {
        Some(ops) => tool.with_unsandboxed_operations(ops),
        None => tool,
    }
}

/// The child snapshot's tool set. A subagent inherits the dispatching
/// session's access authority live (B4: no ungated bypass), so every
/// enforcing surface here reads the same authority as the Captain's own
/// tools:
///
/// - Bash rides the shared sandboxed backend with the same mode and network
///   resolvers plus the same unsandboxed backend: confined exactly like the
///   Captain under `sandbox`, bare exactly when the Captain runs
///   `full-access`. The per-call grant cell is NOT shared — an escalation
///   stamp belongs to the call that requested it.
/// - Write/Edit carry the process write lock (`fence(FileLocked(tool))`, so
///   the fence judges before the lock is taken) and the same fs containment
///   verdict.
/// - Py is deliberately absent: the interpreter runs in-process with no OS
///   sandbox, so its only fence would be the host-side escalation round
///   trip, which the child snapshot does not carry.
///
/// This is the shared fallback shape (no escalation channel); the child
/// authority arms each run at spawn with per-run replacements carrying the
/// parent-channel escalation state.
pub(super) fn child_snapshot(
    bash: BashTool,
    mode_resolver: &Arc<dyn Fn() -> AccessMode + Send + Sync>,
    granted_roots: &crate::granted_roots::GrantedRoots,
) -> Vec<Arc<dyn HarnessAgentTool>> {
    vec![
        Arc::new(manox_harness::read::SelectorReadTool::new()),
        Arc::new(manox_harness::tools::grep::GrepTool),
        Arc::new(manox_harness::tools::glob::GlobTool),
        Arc::new(manox_harness::tools::ls::LsTool),
        Arc::new(SubagentBashTool {
            inner: Arc::new(bash),
            escalation: None,
        }),
        Arc::new(SubagentWriteFence {
            inner: Arc::new(crate::file_lock::FileLockedTool::new(
                Arc::new(manox_harness::tools::write::WriteTool),
                "sailor",
            )),
            mode_resolver: Arc::clone(mode_resolver),
            granted_roots: granted_roots.clone(),
            escalation: None,
        }),
        Arc::new(SubagentWriteFence {
            inner: Arc::new(crate::file_lock::FileLockedTool::new(
                Arc::new(
                    manox_harness::tools::edit::EditTool::default()
                        .with_enforce_seen_lines(crate::settings::edit().enforce_seen_lines),
                ),
                "sailor",
            )),
            mode_resolver: Arc::clone(mode_resolver),
            granted_roots: granted_roots.clone(),
            escalation: None,
        }),
    ]
}

/// The host `TaskStop`: one registry covers every task kind (Sailors
/// register directly; background bash registers through the session's host
/// observer with an on_stop hook into the producer), so one lookup stops
/// anything the model can name. Unknown ids list the running tasks.
pub(super) struct HostTaskStop;

pub(super) const TASKSTOP_DESCRIPTION: &str = "Stop a background task by id — a background bash \
    or an asynchronously-dispatched Sailor subagent (`sailor_id`). Cancels the task's token; the \
    task settles to Stopped. Idempotent for an already-terminal task.";

/// The host `TaskList`: one line per background task this session owns —
/// background bash and dispatched subagents alike — including finished ones
/// not yet reaped, so the model can recover an id or take stock of what is
/// still running. Read-only; pairs with [`HostTaskStop`] (the same registry
/// both read and stop).
pub(super) struct HostTaskList {
    owner_thread_id: String,
}

pub(super) const TASKLIST_DESCRIPTION: &str = "List this session's background tasks — id, kind, \
    status, and command — including finished ones not yet reaped. `BashOutput` reads a `bash` \
    task; a `subagent` delivers its report as a message when it finishes. `TaskStop` stops either.";

#[async_trait::async_trait]
impl manox_harness::tool::AgentTool for HostTaskList {
    fn name(&self) -> &str {
        crate::tools::TASK_LIST
    }
    fn description(&self) -> &str {
        TASKLIST_DESCRIPTION
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        _params: serde_json::Value,
        _signal: tokio_util::sync::CancellationToken,
        _ctx: &dyn manox_harness::tool::ToolContext,
    ) -> Result<manox_harness::tool::AgentToolResult, manox_harness::tool::ToolError> {
        Ok(manox_harness::tool::AgentToolResult::text(
            crate::background_task::render_task_list(&self.owner_thread_id),
        ))
    }

    async fn execute_with_progress(
        &self,
        tool_call_id: &str,
        params: serde_json::Value,
        signal: tokio_util::sync::CancellationToken,
        ctx: &dyn manox_harness::tool::ToolContext,
        _progress: &dyn manox_harness::tool::ToolProgress,
    ) -> Result<manox_harness::tool::AgentToolResult, manox_harness::tool::ToolError> {
        // TaskList never streams; route both entry points through execute.
        self.execute(tool_call_id, params, signal, ctx).await
    }
}

#[async_trait::async_trait]
impl manox_harness::tool::AgentTool for HostTaskStop {
    fn name(&self) -> &str {
        "TaskStop"
    }
    fn description(&self) -> &str {
        TASKSTOP_DESCRIPTION
    }
    fn is_read_only(&self) -> bool {
        false
    }
    /// Stopping a task mutates session runtime state, not the filesystem:
    /// the tool takes no `cwd` and touches no path. Its authority is the
    /// session-owned registry (a model can only stop its own session's
    /// tasks), so the sandbox has nothing to confine and the path fence
    /// must not rule on it.
    fn mutates_through_fs_paths(&self) -> bool {
        false
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "The background task id to stop"
                }
            },
            "required": ["task_id"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        _signal: tokio_util::sync::CancellationToken,
        _ctx: &dyn manox_harness::tool::ToolContext,
    ) -> Result<manox_harness::tool::AgentToolResult, manox_harness::tool::ToolError> {
        let Some(id) = params["task_id"].as_str() else {
            return Err(manox_harness::tool::ToolError::InvalidArguments(
                "`task_id` is required.".into(),
            ));
        };
        crate::background_task::stop(id)
            .await
            .map_err(manox_harness::tool::ToolError::ExecutionFailed)?;
        Ok(manox_harness::tool::AgentToolResult::text(format!(
            "Stopped background task `{id}`"
        )))
    }

    async fn execute_with_progress(
        &self,
        tool_call_id: &str,
        params: serde_json::Value,
        signal: tokio_util::sync::CancellationToken,
        ctx: &dyn manox_harness::tool::ToolContext,
        _progress: &dyn manox_harness::tool::ToolProgress,
    ) -> Result<manox_harness::tool::AgentToolResult, manox_harness::tool::ToolError> {
        // TaskStop never streams; route both entry points through execute.
        self.execute(tool_call_id, params, signal, ctx).await
    }
}

/// WebExplore (internal webview browser) tool names — opt-in, not default,
/// and never offered to subagents.
pub const WEBEXPLORE_TOOL_NAMES: &[&str] = &[
    "WebExploreOpen",
    "WebExploreNavigate",
    "WebExploreReadText",
    "WebExploreReadDom",
    "WebExploreClick",
    "WebExploreType",
    "WebExploreScroll",
    "WebExploreScreenshot",
    "WebExploreYield",
    "WebExploreClose",
];

/// The default active tool subset: every mounted tool except the browser
/// tool suite (WebExplore), which stays dormant until the user opts in via
/// the composer `+` menu.
pub(super) fn default_active_tool_names(tools: &[Arc<dyn HarnessAgentTool>]) -> Vec<String> {
    let browser: std::collections::HashSet<&str> = WEBEXPLORE_TOOL_NAMES.iter().copied().collect();
    tools
        .iter()
        .map(|t| t.name().to_string())
        .filter(|name| !browser.contains(name.as_str()))
        .collect()
}

/// An opt-in browser tool suite toggled from the composer `+` menu. The
/// engine applies the toggle atomically against the session's authoritative
/// active-tool set, so callers never compute the merged set themselves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BrowserSuite {
    WebExplore,
}

impl BrowserSuite {
    /// The tool names belonging to this suite.
    pub fn tool_names(self) -> &'static [&'static str] {
        match self {
            Self::WebExplore => WEBEXPLORE_TOOL_NAMES,
        }
    }

    /// The closed wire name (the §D.3 setter-note family carries it as a
    /// `String`, like every sibling setter; matches the serde `lowercase`
    /// representation the journal and ReadyInfo already serialize).
    pub fn wire(self) -> &'static str {
        match self {
            Self::WebExplore => "webexplore",
        }
    }

    /// Parse a wire suite name; `None` for anything outside the closed
    /// vocabulary (the gateway answers an error note, never a panic). This
    /// is the host-facing parse of the name the §D.3 setter carries: hosts
    /// that keep the suite as a plain string (the desktop app mirrors) run
    /// it per item and skip the `None`s, so retired slugs in restored state
    /// drop out without a compat read.
    pub fn from_wire(name: &str) -> Option<Self> {
        match name {
            "webexplore" => Some(Self::WebExplore),
            _ => None,
        }
    }
}
/// The full toolset: the kernel's file tools plus the the extension layer bash/sub-agent
/// orchestration (assembly mirrors the `the extension layer` orchestration example).
/// Every tool rides behind the host's [`ApprovalGatedTool`] (the kernel ships
/// no gate — permission policy is a harness concern); `AskUserQuestion` joins
/// ungated because asking the user is itself the interaction.
///
/// Returns the tools, the session-scoped background manager that must
/// attach once the session exists (its steerer and lifecycle hook need a
/// live session handle), and the subagent run observer (published into the
/// engine state so the actor's access-mode apply can propagate a mid-turn
/// flip to the live children).
#[allow(clippy::too_many_arguments)] // actor plumbing: each input is distinct session state
pub(super) fn build_tools(
    cwd: &Path,
    runtime: &ModelRuntime,
    model: Option<&HarnessModel>,
    session_id: &str,
    gate: &Arc<ApprovalGate>,
    question_gate: &Arc<crate::questions::UserQuestionGate>,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
    goal_bridge: Option<&Arc<crate::goal_tools::GoalBridge>>,
    granted_roots: &crate::granted_roots::GrantedRoots,
    bus: &Arc<crate::steer_bus::AgentBus>,
) -> (
    Vec<Arc<dyn HarnessAgentTool>>,
    Arc<BackgroundManager>,
    Arc<SubagentRunObserver>,
) {
    // Bash execution backend: OS-confined one-shot commands when the platform
    // backend is available (seatbelt on macOS, bubblewrap + seccomp on Linux;
    // the per-call file-effect profile follows the effective `AccessMode`;
    // shell state does not persist — the tool's `cwd` parameter pins each
    // call), otherwise the unsandboxed persistent brush shell
    // (permission-gated as always). Background tasks reuse this backend's
    // wrap path, so a non-escalated background task is confined exactly like
    // a foreground call. The per-call effective mode reaches the backend via
    // the `mode_resolver`; the writable roots follow the call's cwd through
    // the shared granted-roots store.
    let sandbox_available = crate::sandbox::is_available();
    let mut background = Arc::new(BackgroundRegistry::new());
    // Shared per-call grant cell: an approved sandbox-escalation stamps the
    // wider mode here for exactly one call; the sandboxed backend's mode
    // resolver reads it before the standing session mode.
    let grant_cell: Arc<std::sync::atomic::AtomicI64> = Arc::new(
        std::sync::atomic::AtomicI64::new(manox_harness::sandbox::NO_GRANT),
    );
    let bash_ops: Arc<dyn manox_harness::tools::bash::BashOperations> = if sandbox_available {
        let sandbox_mode_gate = Arc::clone(gate);
        let cell_for_resolver = Arc::clone(&grant_cell);
        let sandbox_mode_resolver: Arc<dyn Fn() -> AccessMode + Send + Sync> =
            Arc::new(move || {
                let g = cell_for_resolver.load(std::sync::atomic::Ordering::SeqCst);
                if g != manox_harness::sandbox::NO_GRANT {
                    AccessMode::from_i64(g)
                } else {
                    sandbox_mode_gate.mode()
                }
            });
        // The network route follows the session switch live (plus the
        // managed proxy port when one is running) — same per-call read the
        // mode resolver makes.
        let network_gate = Arc::clone(gate);
        let network_resolver: Arc<dyn Fn() -> crate::sandbox::NetworkRoute + Send + Sync> =
            Arc::new(move || network_gate.network_route());
        let ops = Arc::new(crate::sandbox::SandboxedBashOperations::new(
            cwd,
            granted_roots.clone(),
            sandbox_mode_resolver,
            network_resolver,
        ));
        let wrap_ops = Arc::clone(&ops);
        let wrap: manox_harness::bash::background::SandboxCommandBuilder =
            Arc::new(move |command, cwd| wrap_ops.wrap_background(command, cwd));
        // Escalated tasks run in the unsandboxed backend's own shell and
        // env, so `full-access` behaves the same whether the command
        // runs in the foreground or the background.
        let unconfined: manox_harness::bash::background::SandboxCommandBuilder =
            Arc::new(crate::sandbox::UnsandboxedBashOperations::command);
        background = Arc::new(
            BackgroundRegistry::new()
                .with_sandbox(wrap)
                .with_unconfined(unconfined),
        );
        ops
    } else {
        Arc::new(PersistentShellOperations::new(cwd))
    };
    // Subagent snapshot inherits the Captain's access authority: the same
    // seatbelt backend AND the same live resolvers (mode + network), so a
    // Sailor's Bash is confined exactly like the Captain's (B4: no ungated
    // bypass) — and runs bare when the Captain itself runs at full access.
    // The per-call grant cell is deliberately NOT shared: an escalation stamp
    // belongs to the call that requested it, and lending the Captain's grant
    // to a background Sailor would widen a call no human approved.
    let subagent_bash_ops: Arc<dyn manox_harness::tools::bash::BashOperations> =
        Arc::clone(&bash_ops);
    let manager = BackgroundManager::new(Arc::clone(&background));
    // Unsandboxed backend (no confinement): selected per call when the
    // effective mode is `full-access` (the standing session mode, or
    // an approved `require_escalated` grant). Installed only where a
    // seatbelt exists to escape from; on other platforms the default backend
    // is already unsandboxed.
    let unsandboxed_ops: Option<Arc<dyn manox_harness::tools::bash::BashOperations>> =
        sandbox_available.then(|| {
            let ops: Arc<dyn manox_harness::tools::bash::BashOperations> =
                Arc::new(crate::sandbox::UnsandboxedBashOperations::new(cwd));
            ops
        });
    // Standing-mode resolver (the session mode, NOT the grant — the grant is
    // what an escalation requests, so it cannot be its own baseline).
    let standing_gate = Arc::clone(gate);
    let standing_resolver: Arc<dyn Fn() -> AccessMode + Send + Sync> =
        Arc::new(move || standing_gate.mode());
    let escalation_approver: Arc<dyn manox_harness::sandbox::EscalationApprover + Send + Sync> =
        Arc::new(crate::approval::GateEscalationApprover::new(Arc::clone(
            gate,
        )));
    let classifier_gate = Arc::clone(gate);
    let mut bash = BashTool::new(bash_ops, background.clone())
        .with_manager(Arc::clone(&manager))
        .with_sandbox_available(sandbox_available)
        .with_mode_resolver(Arc::clone(&standing_resolver))
        .with_network_resolver(Arc::new(move || classifier_gate.network_route().access()))
        .with_grant_cell(grant_cell)
        .with_escalation_approver(Arc::clone(&escalation_approver));
    if let Some(ops) = &unsandboxed_ops {
        bash = bash.with_unsandboxed_operations(Arc::clone(ops));
    }
    let tools: Vec<Arc<dyn HarnessAgentTool>> = vec![
        // Read with path selectors (`path:N-M` / `:raw` / multi-range);
        // selector-less reads delegate to the kernel ReadTool unchanged.
        Arc::new(manox_harness::read::SelectorReadTool::new()),
        // Write/Edit carry the process write lock for their execution window:
        // concurrent writers to the same path get a named-holder conflict
        // instead of silently clobbering each other (old manox file_lock
        // semantics; owner stays "main" until the team system lands).
        Arc::new(crate::file_lock::FileLockedTool::new(
            Arc::new(manox_harness::tools::write::WriteTool),
            "main",
        )),
        Arc::new(crate::file_lock::FileLockedTool::new(
            Arc::new(
                manox_harness::tools::edit::EditTool::default()
                    .with_enforce_seen_lines(crate::settings::edit().enforce_seen_lines),
            ),
            "main",
        )),
        Arc::new(manox_harness::tools::grep::GrepTool),
        Arc::new(manox_harness::tools::glob::GlobTool),
        Arc::new(manox_harness::tools::ls::LsTool),
        // Py: a short Python script on the embedded interpreter. It is not a
        // file tool — the prompt sends ordinary reads/writes/searches to
        // Read/Write/Edit/Grep/Glob/Ls — and it owns no sandbox: unlike
        // sandbox-confined Bash it has no OS fence to ride, so the gate
        // fails it closed under sandbox and an approved escalation is
        // its only wider road.
        Arc::new(manox_harness::tools::py::PyTool),
        Arc::new(bash),
        Arc::new(BashOutputTool::new(background.clone())),
        Arc::new(HostTaskList {
            owner_thread_id: session_id.to_string(),
        }),
        Arc::new(HostTaskStop),
        Arc::new(crate::web_fetch::WebFetchTool::new()),
    ];
    // Bash rides the OS confinement (the per-call file-effect profile) instead
    // of the host gate whenever a seatbelt is mounted: the mode drives the
    // seatbelt directly, and a `require_escalated` escalation is resolved
    // inside the tool through the host-injected approver (never the gate).
    let confined_bash_auto_allow: Option<crate::approval::AutoAllowResolver> = sandbox_available
        .then(|| {
            let allow: crate::approval::AutoAllowResolver =
                Arc::new(move |name: &str, _params: &serde_json::Value| matches!(name, "Bash"));
            allow
        });
    // Write/Edit carry the host escalation config (shared approver + standing
    // resolver); the per-call grant is a local return value in the gate (no
    // shared cell — Write/Edit run `Parallel`). Every gated wrapper also
    // sees the shared granted roots so the fs fence widens exactly like the
    // seatbelt: same-repo worktree auto-admission plus escalation
    // accumulation, both derived from the call's effective cwd.
    let mut tools: Vec<Arc<dyn HarnessAgentTool>> = tools
        .into_iter()
        .map(|tool| {
            let name = tool.name().to_string();
            let mut wrapper = ApprovalGatedTool::new(tool, Arc::clone(gate))
                .with_granted_roots(granted_roots.clone());
            if let Some(allow) = &confined_bash_auto_allow
                && name == "Bash"
            {
                wrapper = wrapper.with_auto_allow(Arc::clone(allow));
            }
            if matches!(
                name.as_str(),
                crate::tools::WRITE
                    | crate::tools::EDIT
                    | crate::tools::TASK_STOP
                    | crate::tools::PY
            ) {
                wrapper = wrapper.with_escalation(
                    Arc::clone(&escalation_approver),
                    Arc::clone(&standing_resolver),
                );
            }
            Arc::new(wrapper) as Arc<dyn HarnessAgentTool>
        })
        .collect();
    tools.push(Arc::new(AskUserQuestionTool::new(Arc::clone(
        question_gate,
    ))));
    // Execution progress: the model publishes its task list; the snapshot
    // rides PlanUpdated to the context rail. Ungated (mutates nothing on
    // disk).
    tools.push(Arc::new(crate::plan::UpdatePlanTool::new(
        notice_tx.clone(),
    )));
    // Task tools (TaskCreate/TaskList/TaskUpdate/TaskGet) were removed in
    // the tools-optimization cycle — they were retired with the Steer-based
    // team architecture and UpdatePlan provides a strictly better alternative.
    // Goal tools ride ungated like AskUserQuestion — they persist the durable
    // goal contract, not filesystem side effects. Absent when the db is
    // unavailable.
    if let Some(bridge) = goal_bridge {
        tools.push(Arc::new(crate::goal_tools::GetGoalTool::new(Arc::clone(
            bridge,
        ))));
        tools.push(Arc::new(crate::goal_tools::CreateGoalTool::new(
            Arc::clone(bridge),
        )));
        tools.push(Arc::new(crate::goal_tools::UpdateGoalTool::new(
            Arc::clone(bridge),
        )));
    }
    // Browser tools (main-thread host round trips via the facade): the read
    // axis stays ungated; the write axis rides the same permission gate as
    // built-ins.
    tools.push(Arc::new(crate::web_tools::WebExploreReadTextTool::new(
        notice_tx.clone(),
    )));
    tools.push(Arc::new(crate::web_tools::WebExploreReadDomTool::new(
        notice_tx.clone(),
    )));
    tools.push(Arc::new(crate::web_tools::WebExploreScreenshotTool::new(
        notice_tx.clone(),
    )));
    // Host-capability tools (clipboard read / open external): registered
    // only when a capability provider is present — a headless context with
    // no host surface exposes neither tool. The clipboard read is a read;
    // the opener rides the same approval gate as the browser write axis.
    if let Some(opener) = crate::host_tools::append(&mut tools, notice_tx.clone()) {
        tools.push(Arc::new(ApprovalGatedTool::new(opener, Arc::clone(gate))));
    }
    for tool in [
        Arc::new(crate::web_tools::WebExploreOpenTool::new(notice_tx.clone()))
            as Arc<dyn HarnessAgentTool>,
        Arc::new(crate::web_tools::WebExploreNavigateTool::new(
            notice_tx.clone(),
        )),
        Arc::new(crate::web_tools::WebExploreClickTool::new(
            notice_tx.clone(),
        )),
        Arc::new(crate::web_tools::WebExploreTypeTool::new(notice_tx.clone())),
        Arc::new(crate::web_tools::WebExploreScrollTool::new(
            notice_tx.clone(),
        )),
        Arc::new(crate::web_tools::WebExploreYieldTool::new(
            notice_tx.clone(),
        )),
        Arc::new(crate::web_tools::WebExploreCloseTool::new(
            notice_tx.clone(),
        )),
    ] {
        tools.push(Arc::new(
            ApprovalGatedTool::new(tool, Arc::clone(gate)).with_escalation(
                Arc::clone(&escalation_approver),
                Arc::clone(&standing_resolver),
            ),
        ));
    }
    // MCP servers (mcp.json + project/plugin .mcp.json): each advertised tool rides
    // behind the same permission gate as built-ins (remote calls are mutating
    // by default). A registry that never initialized (pre-`manox_agent::init`
    // tests) contributes nothing.
    #[cfg(feature = "mcp")]
    if let Some(registry) = crate::mcp::try_global() {
        for server in registry.servers() {
            for tool in &server.tools {
                let mcp_tool = Arc::new(crate::mcp::napi_tool::McpTool::new(
                    server.name.clone(),
                    tool.clone(),
                    Arc::clone(&server.client),
                ));
                tools.push(Arc::new(ApprovalGatedTool::new(mcp_tool, Arc::clone(gate))));
            }
        }
        // The assembly *is* a mount: record the per-slot generations now so
        // the per-prompt refresh's first drift check on this session sees a
        // matching watermark instead of rebuilding an identical set once.
        #[cfg(feature = "mcp")]
        {
            let mut fingerprint: Vec<(String, u64)> = registry
                .ready_overview()
                .into_iter()
                .map(|slot| (slot.name, slot.generation))
                .collect();
            fingerprint.sort();
            MCP_MOUNT_WATERMARKS
                .lock()
                .unwrap()
                .insert(session_id.to_string(), fingerprint);
        }
    }
    // Embedder-registered tools (the hosting editor's contributions —
    // RegisterSessionTools): the provider is the AgentServer's registry;
    // each tool rides the same approval gate as MCP tools (an embedder
    // tool is a remote call into the host, mutating by default). A context
    // with no embedder provider contributes nothing. A registration naming
    // an already-mounted tool — or one of the fixed names mounted after
    // this block — is skipped loudly: by convention app-layer names are
    // lower_snake_case and native names UpCamelCase, so a collision is a
    // caller breach, never a shadowing. Delegation definitions
    // collision-check themselves against this table when they assemble
    // below. The mounted names are recorded: the record is what the
    // per-prompt refresh strips the old set by (the bare names carry no
    // prefix to match on).
    let mut embedder_mounted: Vec<String> = Vec::new();
    if let Some(provider) = crate::embedder_tools::provider() {
        for tool in provider.tools_for(session_id) {
            let gated = Arc::new(ApprovalGatedTool::new(tool, Arc::clone(gate)));
            let name = gated.name().to_string();
            let taken = tools.iter().any(|t| t.name() == name)
                || embedder_mounted.contains(&name)
                || matches!(
                    name.as_str(),
                    "Steer" | crate::subagent::ListAgentsTool::NAME
                );
            if taken {
                tracing::warn!(
                    tool = %name,
                    session = %session_id,
                    "embedder tool registration collides with a mounted name; skipped"
                );
                continue;
            }
            embedder_mounted.push(name);
            tools.push(gated);
        }
        EMBEDDER_MOUNTED_NAMES
            .lock()
            .unwrap()
            .insert(session_id.to_string(), embedder_mounted);
    }
    // Steer bus: engine-scoped (created in `spawn_engine`), mounted here so
    // the model always has the TeamMember messaging + spawn tool.
    tools.push(Arc::new(crate::steer_bus::SteerTool::new(
        Arc::clone(bus),
        manox_harness::steer_bus::AgentId::Captain,
    )));
    // The subagent scheduling surface: one runtime + spawn provider per
    // session assembly; the whole model-facing surface is the Captain's
    // `Steer` tool (dispatch/continue/guide/abort) plus the `ListAgents`
    // catalog. Definitions resolve internally (persona / tool filter /
    // model / permission ceiling / color) — no per-definition model-facing
    // tool is mounted anymore, and child snapshots strip the surface names
    // (nesting is structurally disabled this iteration).
    let subagent_runtime = SubagentRuntime::new();
    let mut registry = AgentRegistry::new();
    register_defaults(&mut registry);
    // User-authored (~/.claude/agents), project (`.claude/agents` under the
    // session cwd, overriding user) + plugin-provided (`<plugin>/agents/`,
    // namespaced) definition layers over the built-ins; later same-name
    // registrations override earlier ones.
    crate::agent_defs::register_user_and_plugin(&mut registry, Some(cwd));
    // Dedicated per-definition models from the cx providers config's
    // `subagents:` map; an unreadable config warns and leaves subagents
    // inheriting the thread model.
    let overrides: HashMap<String, String> = manox_harness::provider::load_subagent_models(
        manox_harness::provider::default_config_path(),
    )
    .unwrap_or_else(|e| {
        tracing::warn!(
            error = %e,
            "subagent model config unreadable; subagents inherit the thread model"
        );
        HashMap::new()
    });
    for key in overrides.keys() {
        if registry.get(key).is_none() {
            tracing::warn!(
                subagent_type = %key,
                "subagent model config names an unknown subagent type"
            );
        }
    }
    let subagent_observer =
        SubagentRunObserver::new(bus.owner_thread_id().to_string(), notice_tx.clone());
    let provider_registry = crate::provider_glue::global();
    let model_slot = gate.model_slot();
    // The inherit-shaped snapshot (the #965 live-inheritance wiring) doubles
    // as the provider's no-factory fallback and the canonical child tool-name
    // set; per-dispatch declarations build through the factory below, which
    // varies resolver wiring only — never the name set.
    let child_assembly = ChildAssembly {
        gate: Arc::clone(gate),
        standing_resolver: Arc::clone(&standing_resolver),
        shared_ops: Arc::clone(&subagent_bash_ops),
        unsandboxed_ops: unsandboxed_ops.clone(),
        sandbox_available,
        cwd: cwd.to_path_buf(),
        granted_roots: granted_roots.clone(),
    };
    let child_tools = child_assembly.build(None);
    let child_tool_factory: manox_harness::subagent::ChildToolFactory = {
        let child_assembly = child_assembly.clone();
        Arc::new(move |effective: Option<&_>| child_assembly.build(effective))
    };
    let child_snapshot_names: HashSet<String> =
        child_tools.iter().map(|t| t.name().to_string()).collect();
    // The definition catalog: one internal dispatch config per registered
    // definition, its `tools:`/`disallowedTools:` resolved (warn-and-skip)
    // against the child snapshot. The registry name is the `Steer`
    // `to.spawn` value verbatim.
    let catalog = Arc::new(crate::subagent::SubagentCatalog::new(
        registry
            .all()
            .into_iter()
            .map(|def| {
                let (allow, denied) =
                    crate::subagent::resolve_definition_filters(def, &child_snapshot_names);
                // A run always spends at least one turn, so `maxTurns: 0` is
                // an authoring error, not a cap of zero — warn and leave the
                // child unbounded rather than honoring a number that means
                // the opposite of what its author wrote.
                let frontmatter_max_turns = match def.max_turns {
                    Some(0) => {
                        tracing::warn!(
                            agent = %def.name,
                            "agent definition declares maxTurns: 0; a run always spends at least \
                             one turn — treating it as unset"
                        );
                        None
                    }
                    other => other,
                };
                SubagentDefConfig {
                    provider: "spawn".into(),
                    name: def.name.clone(),
                    capability: subagent_capability(def).to_string(),
                    description: def.description.clone(),
                    persona: def.system_prompt.clone(),
                    default_tools: allow,
                    default_denied_tools: denied,
                    frontmatter_model_spec: def.model.clone(),
                    config_model_spec: overrides.get(&def.name).cloned(),
                    // The manifest vocabularies map onto this runtime's at
                    // assembly: an out-of-vocabulary declaration warns (once
                    // per session build) and degrades to the unset default.
                    frontmatter_effort: def
                        .effort
                        .as_deref()
                        .and_then(crate::subagent::declared_effort)
                        .map(str::to_string),
                    frontmatter_isolation: def
                        .isolation
                        .as_deref()
                        .and_then(crate::subagent::declared_isolation),
                    frontmatter_background: def.background,
                    frontmatter_max_turns,
                    color: def.color.clone(),
                    declared_permissions: def.permissions,
                    max_depth: 1,
                }
            })
            .collect(),
    ));
    subagent_runtime.register_delegation_tool(crate::subagent::ListAgentsTool::NAME);
    let spawn_provider = SpawnProvider::new(child_tools)
        .with_model_runtime(runtime.clone())
        .with_model_slot(Arc::clone(&model_slot))
        .with_delegation_names(subagent_runtime.delegation_tool_names())
        // The child journal declares the mode it inherits and takes the
        // parent's mid-turn flips: the same standing resolver the child's
        // enforcing tools read per call. A `sandbox` pin overrides this at
        // dispatch (the spawn start resolves the pin to a constant before
        // the row lands), so the durable face never mirrors a wider mode
        // than the run is confined to.
        .with_access_mode_resolver(Arc::clone(&standing_resolver))
        .with_tool_factory(child_tool_factory)
        // Subagent transcripts persist under the host session root (a
        // subdirectory the sidebar's non-recursive listing never surfaces)
        // so their usage stays accountable.
        .with_observer(
            Arc::clone(&subagent_observer) as Arc<dyn manox_harness::subagent::RunObserver>
        )
        // The per-run escalation authority: each child run gets its own
        // grant cell, parent-channel approver, and granted-roots overlay —
        // one Sailor's approved escalation widens that run alone, never a
        // sibling's call or the Captain's.
        .with_child_authority(Arc::new(SubagentChildAuthority::new(
            Arc::clone(gate),
            cwd.to_path_buf(),
            granted_roots.clone(),
            sandbox_available,
            unsandboxed_ops.clone(),
        )));
    let spawn_provider = match model {
        Some(model) => spawn_provider.with_model(model.clone()),
        None => spawn_provider,
    };
    subagent_runtime.register_provider_permanent(Arc::new(spawn_provider));
    let subagent_env: Arc<dyn manox_harness::env::ExecutionEnv> = Arc::new(
        manox_harness::env::TokioExecutionEnv::new(cwd.to_path_buf()),
    );
    // The Steer surface's dispatch arm: the runtime + observer handles and
    // the definition catalog, installed on the engine-scoped bus (it
    // survives session swaps; each assembly replaces the surface).
    bus.install_delegation(crate::steer_bus::SubagentDelegation {
        runtime: Arc::clone(&subagent_runtime),
        observer: Arc::clone(&subagent_observer),
        dispatcher: Arc::new(crate::subagent::SubagentDispatcher {
            observer: Arc::clone(&subagent_observer),
            provider_registry: Some(Arc::clone(&provider_registry)),
            env: Arc::clone(&subagent_env),
            cwd: cwd.to_path_buf(),
            parent_depth: 0,
            parent_session: Some(bus.owner_thread_id().to_string()),
        }),
        catalog: Arc::clone(&catalog),
    });
    tools.push(Arc::new(crate::subagent::ListAgentsTool::new(
        Arc::clone(&subagent_runtime),
        Arc::clone(&subagent_observer),
        Arc::clone(&catalog),
    )));
    (tools, manager, subagent_observer)
}

/// Re-mount the embedder's registered tools (the `RegisterSessionTools`
/// store) onto a live session (#805 companion).
///
/// `build_tools` consults the provider once per session assembly, but the
/// VS Code host registers tools only after it learns the session id —
/// i.e. after Open/Create has already spawned this engine — so the
/// assembly-time snapshot is stale by construction: the client adapters
/// reach neither the model's schema nor `execute_one`'s dispatch table,
/// and a model call fails with `Tool not found: <name>`. The
/// handler's own comment promised "the NEXT tool assembly" sees a
/// registration; for a long-lived session that assembly never came.
///
/// Called on every `Prompt` before the run starts (idle boundary, like
/// the `Open`/`NewSession` re-binds this mirrors), it replaces the
/// previously mounted embedder adapters with the provider's current
/// set — preserving their position in the tool order so a narrow active
/// selection keeps matching by name — and widens an existing selection
/// to cover a fresh registration (new adapters were active at first
/// assembly through `default_active_tool_names`, so a never-narrowed
/// session keeps that behavior without persisting anything). Fail-soft:
/// a rejected re-mount logs and the turn runs on the existing table.
pub(super) async fn refresh_embedder_tools(
    session: &mut AgentSession,
    session_id: &str,
    gate: &Arc<ApprovalGate>,
) {
    let Some(provider) = crate::embedder_tools::provider() else {
        return;
    };
    let mounted = session.mounted_tools();
    // The embedder set currently mounted, by name: the record the last
    // accepted mount (assembly or refresh) left. The bare names carry no
    // prefix to match on, so the record IS the discriminator — stripping
    // the recorded set cannot disturb a built-in.
    let old_embedder: Vec<String> = EMBEDDER_MOUNTED_NAMES
        .lock()
        .unwrap()
        .get(session_id)
        .cloned()
        .unwrap_or_default();
    let old_set: std::collections::HashSet<&str> =
        old_embedder.iter().map(String::as_str).collect();
    let fresh: Vec<Arc<dyn HarnessAgentTool>> = provider
        .tools_for(session_id)
        .into_iter()
        .map(|tool| {
            Arc::new(ApprovalGatedTool::new(tool, Arc::clone(gate))) as Arc<dyn HarnessAgentTool>
        })
        .collect();
    // Collision backstop: a fresh name a non-embedder mount (or an
    // earlier sibling of the same fresh set) already owns is skipped
    // loudly. The naming convention (app-layer lower_snake_case vs
    // native UpCamelCase) makes this a caller breach, never a shadowing.
    let mut fresh_names: Vec<String> = Vec::with_capacity(fresh.len());
    let mut fresh_mounted: Vec<Arc<dyn HarnessAgentTool>> = Vec::with_capacity(fresh.len());
    for tool in fresh {
        let name = tool.name().to_string();
        let taken = mounted
            .iter()
            .any(|t| t.name() == name.as_str() && !old_set.contains(t.name()))
            || fresh_names.contains(&name);
        if taken {
            tracing::warn!(
                tool = %name,
                session = %session_id,
                "embedder tool registration collides with a mounted name; skipped"
            );
            continue;
        }
        fresh_names.push(name);
        fresh_mounted.push(tool);
    }
    let fresh = fresh_mounted;
    if old_embedder.is_empty() && fresh.is_empty() {
        // The host-less / host-idle case: nothing embedder-shaped mounted
        // and nothing registered — skip the rebuild so a plain CLI prompt
        // pays no per-prompt allocation or prompt rebuild.
        return;
    }
    if old_embedder == fresh_names {
        // Identical registration (the extension re-sends the same set on
        // activation): the mounted adapters already route to the current
        // store state — no re-mount needed.
        return;
    }
    let mut tools = Vec::with_capacity(mounted.len() + fresh.len());
    let mut injected = false;
    for tool in mounted {
        if old_set.contains(tool.name()) {
            // Drop every old adapter and inject the fresh set at the old
            // set's first slot, preserving tool order.
            if !injected {
                tools.extend(fresh.iter().cloned());
                injected = true;
            }
        } else {
            tools.push(tool);
        }
    }
    if !injected && !fresh.is_empty() {
        // No embedder tools mounted yet (registration landed after the
        // assembly): append at the end, mirroring build_tools' order.
        tools.extend(fresh);
    }
    if let Err(err) = session.set_tools(tools) {
        tracing::warn!(error = %err, "embedder tool refresh rejected; using the existing table");
        return;
    }
    // The record only moves on an accepted mount — a rejected table
    // leaves it pointing at the set the turn actually runs on.
    EMBEDDER_MOUNTED_NAMES
        .lock()
        .unwrap()
        .insert(session_id.to_string(), fresh_names.clone());
    // A narrowed active selection was computed against the OLD set: carry
    // the non-embedder names over verbatim and replace the embedder
    // names with the fresh ones — mirroring the all-active behavior
    // `default_active_tool_names` gave a registration at first assembly.
    // Persisted only on a real change; a never-narrowed session reports
    // `None` and needs nothing.
    if let Some(active) = session.active_tool_names() {
        let mut next: Vec<String> = active
            .iter()
            .filter(|n| !old_set.contains(n.as_str()))
            .cloned()
            .collect();
        next.extend(fresh_names.iter().cloned());
        let mut cur_sorted = active.clone();
        let mut next_sorted = next.clone();
        cur_sorted.sort_unstable();
        next_sorted.sort_unstable();
        if cur_sorted != next_sorted
            && let Err(err) = session.set_active_tools(next).await
        {
            tracing::warn!(
                error = %err,
                "embedder tool active-selection update rejected; model may not see an embedder tool"
            );
        }
    }
}

/// Per-session record of the embedder tool names currently mounted — the
/// discriminator `refresh_embedder_tools` strips the previous set by (the
/// bare names follow the app-layer lower_snake_case convention and carry
/// no prefix to match on). Updated only on accepted mounts, so a rejected
/// table never strands the record ahead of what the session runs on.
static EMBEDDER_MOUNTED_NAMES: std::sync::LazyLock<std::sync::Mutex<HashMap<String, Vec<String>>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

/// Per-session snapshot of the per-slot generations at the last MCP tool
/// mount/rebuild. Generations are **per slot**, so a single max is not a
/// fingerprint — restarting the lower-generation server while a
/// higher-generation one exists would leave a max-based watermark unchanged
/// and the stale adapters on the cancelled client would survive. The
/// fingerprint is the full `(server, generation)` list.
#[cfg(feature = "mcp")]
pub(super) type MountFingerprint = Vec<(String, u64)>;
#[cfg(feature = "mcp")]
pub(super) static MCP_MOUNT_WATERMARKS: std::sync::LazyLock<
    std::sync::Mutex<HashMap<String, MountFingerprint>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

/// Whether the mounted MCP tool set needs a rebuild: the bridged names moved,
/// or any slot's generation changed since the last mount (a restart with an
/// unchanged list still invalidates the mounted adapters — they hold the
/// cancelled client).
#[cfg(feature = "mcp")]
pub(super) fn mcp_tool_drift(
    old_names: &[String],
    new_names: &[String],
    seen: &[(String, u64)],
    fingerprint: &[(String, u64)],
) -> bool {
    let mut old_sorted = old_names.to_vec();
    let mut new_sorted = new_names.to_vec();
    old_sorted.sort();
    new_sorted.sort();
    old_sorted != new_sorted || seen != fingerprint
}

/// Re-mount the `mcp__`-prefixed tools when the registry's inventory has
/// drifted from the mounted table — a start/stop (from the AHP face or the
/// settings panel) landed since assembly. Same per-prompt shape as
/// [`refresh_embedder_tools`]: the registry is consulted on every run, the
/// drift check is names + generation watermark (no tool-body clones), and
/// only real drift pays for a rebuild.
///
/// A rebuilt set changes the request's tool schema, which transparently
/// breaks the provider prefix cache once — accepted; history is never
/// rewritten.
#[cfg(feature = "mcp")]
pub(super) async fn refresh_mcp_tools(
    session: &mut AgentSession,
    thread_id: &str,
    gate: &Arc<ApprovalGate>,
) {
    let Some(registry) = crate::mcp::try_global() else {
        return;
    };
    let mounted = session.mounted_tools();
    // Same name contract `build_tools` mounted (`mcp__<server>__<tool>`), so
    // prefix-stripping the old set cannot disturb a built-in or a client
    // tool.
    let old_mcp: Vec<String> = mounted
        .iter()
        .filter(|t| t.name().starts_with("mcp__"))
        .map(|t| t.name().to_string())
        .collect();
    let overview = registry.ready_overview();
    let mut new_mcp: Vec<String> = overview
        .iter()
        .flat_map(|slot| {
            slot.tool_names
                .iter()
                .map(|tool| crate::mcp::napi_tool::bridged_tool_name(&slot.name, tool))
        })
        .collect();
    new_mcp.sort();
    let mut fingerprint: Vec<(String, u64)> = overview
        .iter()
        .map(|slot| (slot.name.clone(), slot.generation))
        .collect();
    fingerprint.sort();
    let seen = MCP_MOUNT_WATERMARKS
        .lock()
        .unwrap()
        .get(thread_id)
        .cloned()
        .unwrap_or_default();
    if !mcp_tool_drift(&old_mcp, &new_mcp, &seen, &fingerprint) {
        // No drift: skip the rebuild entirely (and the tool-body clones it
        // would pay).
        return;
    }
    let mut fresh: Vec<Arc<dyn HarnessAgentTool>> = Vec::new();
    for server in registry.servers() {
        for tool in server.tools {
            let mcp_tool = Arc::new(crate::mcp::napi_tool::McpTool::new(
                server.name.clone(),
                tool,
                Arc::clone(&server.client),
            ));
            fresh.push(Arc::new(ApprovalGatedTool::new(mcp_tool, Arc::clone(gate)))
                as Arc<dyn HarnessAgentTool>);
        }
    }
    let mut tools = Vec::with_capacity(mounted.len() + fresh.len());
    let mut injected = false;
    for tool in mounted {
        if tool.name().starts_with("mcp__") {
            if !injected {
                tools.extend(fresh.iter().cloned());
                injected = true;
            }
        } else {
            tools.push(tool);
        }
    }
    if !injected && !fresh.is_empty() {
        tools.extend(fresh);
    }
    if let Err(err) = session.set_tools(tools) {
        tracing::warn!(error = %err, "mcp tool refresh rejected; using the existing table");
        return;
    }
    MCP_MOUNT_WATERMARKS
        .lock()
        .unwrap()
        .insert(thread_id.to_string(), fingerprint);
    // A narrowed active selection was computed against the OLD set: carry
    // the non-MCP names over verbatim, keep every MCP name the user had
    // active (a deliberate narrowing must survive a refresh), and add only
    // the *newly appeared* MCP tools.
    if let Some(active) = session.active_tool_names() {
        let active_set: std::collections::HashSet<&str> =
            active.iter().map(String::as_str).collect();
        let old_set: std::collections::HashSet<&str> = old_mcp.iter().map(String::as_str).collect();
        let mut next: Vec<String> = active
            .iter()
            .filter(|n| !n.starts_with("mcp__"))
            .cloned()
            .collect();
        for name in &new_mcp {
            if active_set.contains(name.as_str()) || !old_set.contains(name.as_str()) {
                next.push(name.clone());
            }
        }
        let mut cur_sorted = active.clone();
        let mut next_sorted = next.clone();
        cur_sorted.sort_unstable();
        next_sorted.sort_unstable();
        if cur_sorted != next_sorted
            && let Err(err) = session.set_active_tools(next).await
        {
            tracing::warn!(
                error = %err,
                "mcp tool active-selection update rejected; model may not see an mcp tool"
            );
        }
    }
}

#[cfg(all(test, feature = "mcp"))]
mod mcp_drift_tests {
    use super::mcp_tool_drift;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_restart_of_the_lower_generation_slot_still_reads_as_drift() {
        // Two servers; A was restarted twice (gen 2), B once (gen 1). The
        // max-based watermark equalled 2; restarting B (gen 1 → 2) leaves
        // the max untouched — the per-slot fingerprint is what catches it.
        let seen = vec![("a".to_string(), 2u64), ("b".to_string(), 1u64)];
        let after_b_restart = vec![("a".to_string(), 2u64), ("b".to_string(), 2u64)];
        let same_names = names(&["mcp__a__x", "mcp__b__y"]);
        assert!(
            mcp_tool_drift(&same_names, &same_names, &seen, &after_b_restart),
            "an unchanged name list with a per-slot generation change is drift"
        );
    }

    #[test]
    fn an_unchanged_fingerprint_is_not_drift() {
        let seen = vec![("a".to_string(), 2u64), ("b".to_string(), 1u64)];
        let same = seen.clone();
        let same_names = names(&["mcp__a__x", "mcp__b__y"]);
        assert!(!mcp_tool_drift(&same_names, &same_names, &seen, &same));
    }

    #[test]
    fn a_name_change_is_drift_regardless_of_generations() {
        let seen = vec![("a".to_string(), 1u64)];
        let fingerprint = seen.clone();
        assert!(mcp_tool_drift(
            &names(&["mcp__a__x"]),
            &names(&["mcp__a__x", "mcp__a__y"]),
            &seen,
            &fingerprint
        ));
    }

    #[test]
    fn a_newly_enabled_server_is_drift_even_at_generation_zero() {
        // The enable path creates the slot at generation 0 and start bumps
        // it — but even a generation-0 slot must count against a fingerprint
        // that did not carry the server at all.
        let seen: Vec<(String, u64)> = Vec::new();
        let fingerprint = vec![("b".to_string(), 0u64)];
        assert!(mcp_tool_drift(
            &[],
            &names(&["mcp__b__y"]),
            &seen,
            &fingerprint
        ));
    }
}
