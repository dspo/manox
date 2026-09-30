//! Engine tool assembly: the subagent/monitor/task-stop tools, browser
//! suites, `build_tools`, the session orchestrators, and the embedder/MCP
//! refresh paths.

use super::*;

/// Infer a capability tag for an agent definition from its declared tool
/// allowlist. The host snapshot carries Read/Grep/Glob/Ls (read),
/// Write/Edit (write), and Bash (exec); `tools: []` means the full
/// snapshot. The tag rides the delegation tool's description so the model
/// knows what each subagent can do before dispatching.
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
/// Reject `run_in_background` outright; the description is also corrected
/// for the subagent context (one-shot, no state persistence, no gating
/// claim) since the kernel BashTool's static text assumes the host session.
pub(super) struct SubagentBashTool {
    pub(super) inner: Arc<dyn manox_harness::tool::AgentTool>,
}

pub(super) const SUBAGENT_BASH_DESCRIPTION: &str = "Execute a shell command. Each call runs in a fresh \
    one-shot shell at the current cwd (no persistent cwd/vars across calls), under the same \
    backend as the Captain (seatbelt-confined where the host has one). `run_in_background` is \
    not available inside a subagent — the subagent itself is the async primitive, so run long \
    commands in the foreground. Use `head_lines`/`tail_lines` to keep a selection of output \
    instead of piping through `head`/`tail`.";

#[async_trait::async_trait]
impl manox_harness::tool::AgentTool for SubagentBashTool {
    fn name(&self) -> &str {
        "Bash"
    }
    fn description(&self) -> &str {
        SUBAGENT_BASH_DESCRIPTION
    }
    fn is_read_only(&self) -> bool {
        false
    }
    fn requires_approval(&self, params: &serde_json::Value) -> bool {
        self.inner.requires_approval(params)
    }
    fn execution_mode(&self) -> manox_harness::tool::ExecutionMode {
        // Delegate: the kernel BashTool declares Sequential (a stateful
        // persistent shell on non-macOS); the wrapper must inherit it so a
        // single Sailor session doesn't interleave parallel bash state.
        self.inner.execution_mode()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        // Strip `run_in_background` (refused by this wrapper — N1) and the
        // `sandbox_permissions`/`justification` escalation fields (no
        // escalation path in the ungated subagent session) so the model
        // never proposes them.
        let mut schema = self.inner.parameters_schema();
        if let Some(props) = schema.get_mut("properties").and_then(|p| p.as_object_mut()) {
            props.remove("run_in_background");
            props.remove("sandbox_permissions");
            props.remove("justification");
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
        if params["run_in_background"].as_bool().unwrap_or(false) {
            return Err(manox_harness::tool::ToolError::ExecutionFailed(
                "`run_in_background` is not available inside a subagent session — the subagent \
                 itself is the async primitive. Run the command in the foreground instead."
                    .into(),
            ));
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
        if params["run_in_background"].as_bool().unwrap_or(false) {
            return Err(manox_harness::tool::ToolError::ExecutionFailed(
                "`run_in_background` is not available inside a subagent session — the subagent \
                 itself is the async primitive. Run the command in the foreground instead."
                    .into(),
            ));
        }
        self.inner
            .execute_with_progress(tool_call_id, params, signal, ctx, progress)
            .await
    }
}

/// The host `TaskStop`: one registry covers every task kind (Sailors
/// register directly; bash/monitor/ws register through the session's host
/// observer with an on_stop hook into the producer), so one lookup stops
/// anything the model can name. Unknown ids list the running tasks.
pub(super) struct HostTaskStop;

pub(super) const TASKSTOP_DESCRIPTION: &str = "Stop a background task by id — a background bash, a monitor, \
    or an asynchronously-dispatched Sailor subagent (`sailor_id`). Cancels the task's token; the \
    task settles to Stopped. Idempotent for an already-terminal task.";

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

/// ChromeUse tool names — opt-in via the composer `+` menu, never in the
/// default active set, and never offered to subagents.
pub const CHROMEUSE_TOOL_NAMES: &[&str] = &[
    "ChromeUseOpen",
    "ChromeUseNavigate",
    "ChromeUseHover",
    "ChromeUseClick",
    "ChromeUseType",
    "ChromeUsePressKey",
    "ChromeUseSelectOption",
    "ChromeUseScroll",
    "ChromeUseSnapshot",
    "ChromeUseWaitFor",
    "ChromeUseScreenshot",
    "ChromeUseTabs",
    "ChromeUseEvaluate",
    "ChromeUseClose",
    "ChromeUseFindChromiumExecutable",
];
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
/// tool suites (ChromeUse + WebExplore), which stay dormant until the user
/// opts in via the composer `+` menu.
pub(super) fn default_active_tool_names(tools: &[Arc<dyn PiAgentTool>]) -> Vec<String> {
    let browser: std::collections::HashSet<&str> = CHROMEUSE_TOOL_NAMES
        .iter()
        .chain(WEBEXPLORE_TOOL_NAMES)
        .copied()
        .collect();
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
    ChromeUse,
    WebExplore,
}

impl BrowserSuite {
    /// The tool names belonging to this suite.
    pub fn tool_names(self) -> &'static [&'static str] {
        match self {
            Self::ChromeUse => CHROMEUSE_TOOL_NAMES,
            Self::WebExplore => WEBEXPLORE_TOOL_NAMES,
        }
    }

    /// The closed wire name (the §D.3 setter-note family carries it as a
    /// `String`, like every sibling setter; matches the serde `lowercase`
    /// representation the journal and ReadyInfo already serialize).
    pub fn wire(self) -> &'static str {
        match self {
            Self::ChromeUse => "chromeuse",
            Self::WebExplore => "webexplore",
        }
    }

    /// Parse a wire suite name; `None` for anything outside the closed
    /// vocabulary (the gateway answers an error note, never a panic).
    pub fn from_wire(name: &str) -> Option<Self> {
        match name {
            "chromeuse" => Some(Self::ChromeUse),
            "webexplore" => Some(Self::WebExplore),
            _ => None,
        }
    }
}
/// The full pi toolset: pi's file tools plus the pi-extensions bash/sub-agent
/// orchestration (assembly mirrors the `pi-extensions` orchestration example).
/// Every tool rides behind the host's [`ApprovalGatedTool`] (the kernel ships
/// no gate — permission policy is a harness concern); `AskUserQuestion` joins
/// ungated because asking the user is itself the interaction.
///
/// Returns the tools plus the session-scoped orchestrators that must attach
/// once the session exists (their steerers and lifecycle hooks need a live
/// session handle).
#[allow(clippy::too_many_arguments)] // actor plumbing: each input is distinct session state
pub(super) fn build_tools(
    cwd: &Path,
    runtime: &ModelRuntime,
    model: Option<&PiModel>,
    session_id: &str,
    gate: &Arc<ApprovalGate>,
    question_gate: &Arc<crate::questions::UserQuestionGate>,
    plan: &Arc<crate::plan_mode::PlanSessionState>,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
    goal_bridge: Option<&Arc<crate::goal_tools::GoalBridge>>,
    granted_roots: &crate::granted_roots::GrantedRoots,
    bus: &Arc<crate::steer_bus::AgentBus>,
) -> (
    Vec<Arc<dyn PiAgentTool>>,
    SessionOrchestrators,
    crate::plan_mode::ReadOnlySubagentResolver,
) {
    // Bash execution backend: seatbelt-wrapped one-shot commands when the
    // OS backend is available (the per-call file-effect profile is rendered
    // from the effective `PermissionMode`; shell state does not persist —
    // the tool's `cwd` parameter pins each call), otherwise the unsandboxed
    // persistent brush shell (permission-gated as always). Background tasks
    // reuse this backend's `wrap_command`, so a non-escalated background
    // task is confined exactly like a foreground call. The per-call effective
    // mode reaches the seatbelt via the `mode_resolver`; the writable roots
    // follow the call's cwd through the shared granted-roots store.
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
        let sandbox_mode_resolver: Arc<dyn Fn() -> PermissionMode + Send + Sync> =
            Arc::new(move || {
                let g = cell_for_resolver.load(std::sync::atomic::Ordering::SeqCst);
                if g != manox_harness::sandbox::NO_GRANT {
                    PermissionMode::from_i64(g)
                } else {
                    sandbox_mode_gate.mode()
                }
            });
        let ops = Arc::new(crate::sandbox::SandboxedBashOperations::new(
            cwd,
            granted_roots.clone(),
            sandbox_mode_resolver,
        ));
        let wrap_ops = Arc::clone(&ops);
        let wrap: manox_harness::bash::background::SandboxCommandBuilder =
            Arc::new(move |command, cwd| wrap_ops.wrap_background(command, cwd));
        background = Arc::new(BackgroundRegistry::new().with_sandbox(wrap));
        ops
    } else {
        Arc::new(PersistentShellOperations::new(cwd))
    };
    // Subagent snapshot inherits the same seatbelt backend so a Sailor's
    // Bash is confined exactly like the Captain's (B4: no ungated bypass).
    let subagent_bash_ops: Arc<dyn manox_harness::tools::bash::BashOperations> =
        Arc::clone(&bash_ops);
    let subagent_background = Arc::new(BackgroundRegistry::new());
    let manager = BackgroundManager::new(Arc::clone(&background));
    let monitor = MonitorManager::new(Arc::clone(&background));
    // Unsandboxed backend (no confinement): selected per call when the
    // effective mode is `danger-full-access` (the standing session mode, or
    // an approved `sandbox_permissions` grant). Installed only where a
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
    let standing_resolver: Arc<dyn Fn() -> PermissionMode + Send + Sync> =
        Arc::new(move || standing_gate.mode());
    let escalation_approver: Arc<dyn manox_harness::sandbox::EscalationApprover + Send + Sync> =
        Arc::new(crate::approval::GateEscalationApprover::new(Arc::clone(
            gate,
        )));
    let mut bash = BashTool::new(bash_ops, background.clone())
        .with_manager(Arc::clone(&manager))
        .with_sandbox_available(sandbox_available)
        .with_mode_resolver(Arc::clone(&standing_resolver))
        .with_grant_cell(grant_cell)
        .with_escalation_approver(Arc::clone(&escalation_approver));
    if let Some(ops) = unsandboxed_ops {
        bash = bash.with_unsandboxed_operations(ops);
    }
    let tools: Vec<Arc<dyn PiAgentTool>> = vec![
        // Read with oh-my-pi path selectors (`path:N-M` / `:raw` / multi-range);
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
        Arc::new(bash),
        Arc::new(MonitorTool::new(Arc::clone(&monitor))),
        Arc::new(BashOutputTool::new(background.clone())),
        Arc::new(HostTaskStop),
        Arc::new(crate::web_fetch::WebFetchTool::new()),
    ];
    // Plan-mode gate exemption: plan-file writes stay ungated while
    // plan mode is active (the `ToolCall` hook blocks everything else).
    let plan_policy = Arc::new(crate::plan_mode::PlanGatePolicy {
        state: Arc::clone(plan),
        plans_dir: crate::paths::plans_dir().unwrap_or_else(|_| PathBuf::from(".manox/plans")),
        cwd: cwd.to_path_buf(),
    });
    // Bash rides the OS confinement (the per-call file-effect profile) instead
    // of the host gate whenever a seatbelt is mounted: the mode drives the
    // seatbelt directly, and a `sandbox_permissions` escalation is resolved
    // inside the tool through the host-injected approver (never the gate).
    // `Monitor`'s command half spawns through the same sandbox-wrapped
    // background registry under workspace-write.
    let confined_bash_auto_allow: Option<crate::approval::AutoAllowResolver> = sandbox_available
        .then(|| {
            let allow: crate::approval::AutoAllowResolver =
                Arc::new(move |name: &str, params: &serde_json::Value| match name {
                    "Bash" => true,
                    "Monitor" => params
                        .get("command")
                        .and_then(|c| c.as_str())
                        .is_some_and(|c| !c.trim().is_empty()),
                    _ => false,
                });
            allow
        });
    // Write/Edit carry the host escalation config (shared approver + standing
    // resolver); the per-call grant is a local return value in the gate (no
    // shared cell — Write/Edit run `Parallel`). Every gated wrapper also
    // sees the shared granted roots so the fs fence widens exactly like the
    // seatbelt: same-repo worktree auto-admission plus escalation
    // accumulation, both derived from the call's effective cwd.
    let mut tools: Vec<Arc<dyn PiAgentTool>> = tools
        .into_iter()
        .map(|tool| {
            let name = tool.name().to_string();
            let mut wrapper = ApprovalGatedTool::new(tool, Arc::clone(gate))
                .with_plan_policy(Arc::clone(&plan_policy))
                .with_granted_roots(granted_roots.clone());
            if let Some(allow) = &confined_bash_auto_allow
                && matches!(name.as_str(), "Bash" | "Monitor")
            {
                wrapper = wrapper.with_auto_allow(Arc::clone(allow));
            }
            if matches!(name.as_str(), "Write" | "Edit" | "TaskStop") {
                wrapper = wrapper.with_escalation(
                    Arc::clone(&escalation_approver),
                    Arc::clone(&standing_resolver),
                );
            }
            Arc::new(wrapper) as Arc<dyn PiAgentTool>
        })
        .collect();
    tools.push(Arc::new(
        PiAskUserQuestionTool::new(Arc::clone(question_gate)).with_plan_state(Arc::clone(plan)),
    ));
    // Plan proposal rides ungated like AskUserQuestion: submitting a plan is
    // the verdict request itself, not a side effect.
    tools.push(Arc::new(crate::plan_mode::ProposePlanTool::new(
        notice_tx.clone(),
        Arc::clone(plan),
        plan_policy.plans_dir.clone(),
    )));
    // Execution progress: the model publishes its task list; the snapshot
    // rides PlanUpdated to the context rail. Ungated (mutates nothing on
    // disk); plan mode's ToolCall hook blocks it while planning.
    tools.push(Arc::new(crate::plan::UpdatePlanTool::new(
        notice_tx.clone(),
    )));
    // Task tools (TaskCreate/TaskList/TaskUpdate/TaskGet) were removed in
    // the tools-optimization cycle — they were retired with the Steer-based
    // team architecture and UpdatePlan provides a strictly better alternative.
    // AskUserQuestion/ProposePlan — they persist the durable goal contract,
    // not filesystem side effects. Absent when the db is unavailable.
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
    // built-ins. Plan mode's ToolCall hook blocks both (fixed allowlist).
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
        tools.push(Arc::new(
            ApprovalGatedTool::new(opener, Arc::clone(gate))
                .with_plan_policy(Arc::clone(&plan_policy)),
        ));
    }
    for tool in [
        Arc::new(crate::web_tools::WebExploreOpenTool::new(notice_tx.clone()))
            as Arc<dyn PiAgentTool>,
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
            ApprovalGatedTool::new(tool, Arc::clone(gate))
                .with_plan_policy(Arc::clone(&plan_policy))
                .with_escalation(
                    Arc::clone(&escalation_approver),
                    Arc::clone(&standing_resolver),
                ),
        ));
    }
    // ChromeUse (real Chrome via the in-process rustwright CDP engine): same
    // trust axes as WebExplore — reads stay ungated; writes ride the approval
    // gate. Plan mode's ToolCall hook blocks both (fixed allowlist). Compiled
    // only with the `chrome-use` feature: the VS Code host builds the agent
    // without it, so the engine is never linked into the extension.
    #[cfg(feature = "chrome-use")]
    {
        tools.push(Arc::new(crate::chrome_use::ChromeUseSnapshotTool));
        tools.push(Arc::new(crate::chrome_use::ChromeUseWaitForTool));
        tools.push(Arc::new(crate::chrome_use::ChromeUseScreenshotTool));
        tools.push(Arc::new(
            crate::chrome_use::ChromeUseFindChromiumExecutableTool,
        ));
        for tool in [
            Arc::new(crate::chrome_use::ChromeUseOpenTool) as Arc<dyn PiAgentTool>,
            Arc::new(crate::chrome_use::ChromeUseNavigateTool),
            Arc::new(crate::chrome_use::ChromeUseHoverTool),
            Arc::new(crate::chrome_use::ChromeUseClickTool),
            Arc::new(crate::chrome_use::ChromeUseTypeTool),
            Arc::new(crate::chrome_use::ChromeUsePressKeyTool),
            Arc::new(crate::chrome_use::ChromeUseSelectOptionTool),
            Arc::new(crate::chrome_use::ChromeUseScrollTool),
            Arc::new(crate::chrome_use::ChromeUseTabsTool),
            Arc::new(crate::chrome_use::ChromeUseEvaluateTool),
            Arc::new(crate::chrome_use::ChromeUseCloseTool),
        ] {
            tools.push(Arc::new(
                ApprovalGatedTool::new(tool, Arc::clone(gate))
                    .with_plan_policy(Arc::clone(&plan_policy)),
            ));
        }
    }
    // MCP servers (mcp.toml + plugin .mcp.json): each advertised tool rides
    // behind the same permission gate as built-ins (remote calls are mutating
    // by default). A registry that never initialized (pre-`manox_agent::init`
    // tests) contributes nothing.
    #[cfg(feature = "mcp")]
    if let Some(registry) = crate::mcp::try_global() {
        for server in registry.servers() {
            for tool in &server.tools {
                let mcp_tool = Arc::new(crate::mcp::napi_tool::PiMcpTool::new(
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
    // with no embedder provider contributes nothing.
    if let Some(provider) = crate::embedder_tools::provider() {
        for tool in provider.tools_for(session_id) {
            tools.push(Arc::new(ApprovalGatedTool::new(tool, Arc::clone(gate))));
        }
    }
    // Steer bus: engine-scoped (created in `spawn_engine`), mounted here so
    // the model always has the TeamMember messaging + spawn tool.
    tools.push(Arc::new(crate::steer_bus::SteerTool::new(
        Arc::clone(bus),
        manox_harness::steer_bus::AgentId::Captain,
    )));
    // The dsh-isomorphic delegation surface: one runtime + spawn provider
    // per session assembly; one delegation tool per registered definition
    // (Explore, Sailor, user/plugin manifests); the control pair. Child
    // snapshots strip every registered delegation-surface name (nesting is
    // structurally disabled this iteration).
    let subagent_runtime = SubagentRuntime::new();
    let mut registry = AgentRegistry::new();
    register_defaults(&mut registry);
    // User-authored (~/.claude/agents) + plugin-provided
    // (`<plugin>/agents/`, namespaced) definitions layer over the
    // built-ins; same-name user files override built-ins.
    crate::agent_defs::register_user_and_plugin(&mut registry);
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
    // Child snapshot: the same seatbelt backend as the Captain (B4: no
    // ungated bypass); Write/Edit carry the process write lock so parallel
    // workers clobbering the same path surface a named-holder conflict
    // instead of silently racing.
    let child_tools: Vec<Arc<dyn PiAgentTool>> = vec![
        Arc::new(manox_harness::read::SelectorReadTool::new()),
        Arc::new(manox_harness::tools::grep::GrepTool),
        Arc::new(manox_harness::tools::glob::GlobTool),
        Arc::new(manox_harness::tools::ls::LsTool),
        Arc::new(SubagentBashTool {
            inner: Arc::new(
                manox_harness::bash::BashTool::new(
                    Arc::clone(&subagent_bash_ops),
                    subagent_background.clone(),
                )
                .with_sandbox_available(sandbox_available),
            ),
        }),
        Arc::new(crate::file_lock::FileLockedTool::new(
            Arc::new(manox_harness::tools::write::WriteTool),
            "sailor",
        )),
        Arc::new(crate::file_lock::FileLockedTool::new(
            Arc::new(
                manox_harness::tools::edit::EditTool::default()
                    .with_enforce_seen_lines(crate::settings::edit().enforce_seen_lines),
            ),
            "sailor",
        )),
    ];
    let child_snapshot_names: HashSet<String> =
        child_tools.iter().map(|t| t.name().to_string()).collect();
    // Delegation tool names: sanitized into the provider wire charset
    // (`[A-Za-z0-9_-]{1,64}` — plugin namespacing's `:` would 400 every
    // request of the session), collision-checked against every model-facing
    // name in this assembly, and registered into the runtime so child
    // snapshots strip them (nesting is structurally disabled this
    // iteration).
    let mut taken_tool_names: HashSet<String> =
        tools.iter().map(|t| t.name().to_string()).collect();
    taken_tool_names.insert(crate::subagent::ListAgentsTool::NAME.to_string());
    taken_tool_names.insert(crate::subagent::InterruptAgentTool::NAME.to_string());
    let delegation_defs: Vec<(
        String,
        &manox_harness::ext_point_agent::AgentDef,
        Vec<String>,
    )> = registry
        .all()
        .into_iter()
        .filter_map(|def| {
            let assembly = crate::subagent::resolve_delegation_tool(
                def,
                &taken_tool_names,
                &child_snapshot_names,
            )?;
            taken_tool_names.insert(assembly.tool_name.clone());
            Some((assembly.tool_name, def, assembly.default_tools))
        })
        .collect();
    for (tool_name, _, _) in &delegation_defs {
        subagent_runtime.register_delegation_tool(tool_name);
    }
    subagent_runtime.register_delegation_tool(crate::subagent::ListAgentsTool::NAME);
    subagent_runtime.register_delegation_tool(crate::subagent::InterruptAgentTool::NAME);
    let spawn_provider = SpawnProvider::new(child_tools)
        .with_model_runtime(runtime.clone())
        .with_model_slot(Arc::clone(&model_slot))
        .with_delegation_names(subagent_runtime.delegation_tool_names())
        // Subagent transcripts persist under the host session root (a
        // subdirectory the sidebar's non-recursive listing never surfaces)
        // so their usage stays accountable.
        .with_session_dir(crate::thread_store::sessions_dir().join("subagents"))
        .with_observer(
            Arc::clone(&subagent_observer) as Arc<dyn manox_harness::subagent::RunObserver>
        );
    let spawn_provider = match model {
        Some(model) => spawn_provider.with_model(model.clone()),
        None => spawn_provider,
    };
    subagent_runtime.register_provider_permanent(Arc::new(spawn_provider));
    let subagent_env: Arc<dyn manox_harness::env::ExecutionEnv> = Arc::new(
        manox_harness::env::TokioExecutionEnv::new(cwd.to_path_buf()),
    );
    // The plan-mode read-only map keys off the SANITIZED wire name the
    // model actually calls — registry names and tool names diverge once
    // plugin namespacing is sanitized away.
    let mut read_only_by_tool: HashMap<String, bool> = HashMap::new();
    for (tool_name, def, allow) in &delegation_defs {
        let capability = subagent_capability(def);
        read_only_by_tool.insert(tool_name.clone(), capability == "read-only");
        let config = DelegationToolConfig {
            provider: "spawn".into(),
            name: tool_name.clone(),
            capability: capability.to_string(),
            rendered_description: crate::subagent::delegation_description(
                &def.description,
                capability,
                tool_name,
            ),
            persona: def.system_prompt.clone(),
            default_tools: allow.clone(),
            frontmatter_model_spec: def.model.clone(),
            config_model_spec: overrides.get(&def.name).cloned(),
            max_depth: 1,
        };
        tools.push(Arc::new(crate::subagent::DelegationTool::new(
            Arc::clone(&subagent_runtime),
            Arc::clone(&subagent_observer),
            config,
            Some(Arc::clone(&provider_registry)),
            0,
            Some(bus.owner_thread_id().to_string()),
            Arc::clone(&subagent_env),
            cwd.to_path_buf(),
        )));
    }
    tools.push(Arc::new(crate::subagent::ListAgentsTool::new(
        Arc::clone(&subagent_runtime),
        Arc::clone(&subagent_observer),
    )));
    tools.push(Arc::new(crate::subagent::InterruptAgentTool::new(
        Arc::clone(&subagent_runtime),
    )));
    // Plan-mode gate resolver: whether a delegation tool name targets a
    // read-only definition, keyed by the sanitized wire name the model
    // calls. Built from the same assembly pass that names the tools, so it
    // cannot diverge from what is actually registered.
    let read_only_subagent: crate::plan_mode::ReadOnlySubagentResolver = {
        let map = Arc::new(read_only_by_tool);
        Arc::new(move |name: &str| map.get(name).copied().unwrap_or(false))
    };
    (
        tools,
        SessionOrchestrators {
            monitor,
            background: manager,
        },
        read_only_subagent,
    )
}

pub(super) struct SessionOrchestrators {
    pub(super) monitor: Arc<MonitorManager>,
    pub(super) background: Arc<BackgroundManager>,
}

/// Re-mount the embedder's registered tools (the `RegisterSessionTools`
/// store) onto a live session (#805 companion).
///
/// `build_tools` consults the provider once per session assembly, but the
/// VS Code host registers tools only after it learns the session id —
/// i.e. after Open/Create has already spawned this engine — so the
/// assembly-time snapshot is stale by construction: the client adapters
/// reach neither the model's schema nor `execute_one`'s dispatch table,
/// and a model call fails with `Tool not found: client_<name>`. The
/// handler's own comment promised "the NEXT tool assembly" sees a
/// registration; for a long-lived session that assembly never came.
///
/// Called on every `Prompt` before the run starts (idle boundary, like
/// the `Open`/`NewSession` re-binds this mirrors), it replaces the
/// previously mounted `client_*` adapters with the provider's current
/// set — preserving their position in the tool order so a narrow active
/// selection keeps matching by name — and widens an existing selection
/// to cover a fresh registration (new adapters were active at first
/// assembly through `default_active_tool_names`, so a never-narrowed
/// session keeps that behavior without persisting anything). Fail-soft:
/// a rejected re-mount (e.g. a host registration colliding with a
/// built-in name) logs and the turn runs on the existing table.
pub(super) async fn refresh_embedder_tools(
    session: &mut AgentSession,
    session_id: &str,
    gate: &Arc<ApprovalGate>,
) {
    let Some(provider) = crate::embedder_tools::provider() else {
        return;
    };
    let mounted = session.mounted_tools();
    // The provider's adapters all carry the `client_` prefix; that is the
    // same name contract `build_tools` mounted them under and the model
    // dispatches against, so stripping the previous embedder set by
    // prefix cannot disturb a built-in.
    let fresh: Vec<Arc<dyn PiAgentTool>> = provider
        .tools_for(session_id)
        .into_iter()
        .map(|tool| {
            Arc::new(ApprovalGatedTool::new(tool, Arc::clone(gate))) as Arc<dyn PiAgentTool>
        })
        .collect();
    let old_client: Vec<String> = mounted
        .iter()
        .filter(|t| t.name().starts_with("client_"))
        .map(|t| t.name().to_string())
        .collect();
    let new_client: Vec<String> = fresh.iter().map(|t| t.name().to_string()).collect();
    if old_client.is_empty() && new_client.is_empty() {
        // The host-less / host-idle case: nothing embedder-shaped mounted
        // and nothing registered — skip the rebuild so a plain CLI prompt
        // pays no per-prompt allocation or prompt rebuild.
        return;
    }
    if old_client == new_client {
        // Identical registration (the extension re-sends the same set on
        // activation): the mounted adapters already route to the current
        // store state — no re-mount needed.
        return;
    }
    let mut tools = Vec::with_capacity(mounted.len() + fresh.len());
    let mut injected = false;
    for tool in mounted {
        if tool.name().starts_with("client_") {
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
        // No client tools mounted yet (registration landed after the
        // assembly): append at the end, mirroring build_tools' order.
        tools.extend(fresh);
    }
    if let Err(err) = session.set_tools(tools) {
        tracing::warn!(error = %err, "embedder tool refresh rejected; using the existing table");
        return;
    }
    // A narrowed active selection was computed against the OLD set: carry
    // the non-client names over verbatim and replace the client names
    // with the fresh ones — mirroring the all-active behavior
    // `default_active_tool_names` gave a registration at first assembly.
    // Persisted only on a real change; a never-narrowed session reports
    // `None` and needs nothing.
    if let Some(active) = session.active_tool_names() {
        let mut next: Vec<String> = active
            .iter()
            .filter(|n| !n.starts_with("client_"))
            .cloned()
            .collect();
        next.extend(new_client.iter().cloned());
        let mut cur_sorted = active.clone();
        let mut next_sorted = next.clone();
        cur_sorted.sort_unstable();
        next_sorted.sort_unstable();
        if cur_sorted != next_sorted
            && let Err(err) = session.set_active_tools(next).await
        {
            tracing::warn!(
                error = %err,
                "embedder tool active-selection update rejected; model may not see a client tool"
            );
        }
    }
}

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
    let mut fresh: Vec<Arc<dyn PiAgentTool>> = Vec::new();
    for server in registry.servers() {
        for tool in server.tools {
            let mcp_tool = Arc::new(crate::mcp::napi_tool::PiMcpTool::new(
                server.name.clone(),
                tool,
                Arc::clone(&server.client),
            ));
            fresh.push(Arc::new(ApprovalGatedTool::new(mcp_tool, Arc::clone(gate)))
                as Arc<dyn PiAgentTool>);
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
