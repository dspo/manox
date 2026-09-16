//! Host-side subagent delegation surface — the dsh `tool-subagent` +
//! `tool-subagent-control` equivalent:
//!
//! - [`DelegationTool`]: one model-facing tool per agent definition (name =
//!   definition name), bound to the `spawn` provider. Foreground by default
//!   (the tool result IS the child's final output); `run_in_background`
//!   parks the run on a background card and delivers the settled result as
//!   a peer report.
//! - [`ListAgentsTool`] / [`InterruptAgentTool`]: the control pair over the
//!   runtime's live-run table.
//! - [`SubagentRunObserver`]: the [`RunObserver`] bridge folding ext-level
//!   run events into this host's transcript rail, health watchdog, and
//!   background cards.
//!
//! The run loop itself lives in the harness's `spawn` provider; everything
//! that touches host-owned surfaces (ThreadEvent notices, the watchdog,
//! background cards, peer delivery) flows through here.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use manox_harness::subagent::{
    Budgets, DelegationToolConfig, RunEndInfo, RunInfo, RunObserver, RunResult, StopReason,
    SubagentRuntime, ToolFilter,
};
use manox_harness::tool::{AgentTool, AgentToolResult, ToolContext, ToolError};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::background_task::{self, TaskKind, TaskStatus};
use crate::subagent_watchdog::SubagentWatchdog;
use crate::thread::ThreadEvent;
use crate::thread_engine::BackendNotice;

/// Prefix of the peer delivery a failed subagent run emits to the parent.
/// Shared with `subagent_restore`, which keys the restored row's status off
/// it: a failed run delivers (unlike an abort), and its row must read as
/// `Error`, not the `Success` a plain delivery implies.
pub const SUBAGENT_FAILED_DELIVERY_PREFIX: &str = "subagent failed: ";

/// Prefix of the peer delivery a timed-out subagent run emits to the parent.
/// Shared with `subagent_restore`, which keys the restored row's status off
/// it: a timed-out run delivers (unlike an abort), and its row must read as
/// `Error`, not the `Success` a plain delivery implies.
pub const SUBAGENT_TIMED_OUT_DELIVERY_PREFIX: &str = "subagent timed out: ";

/// Sanitize a definition's registry name into a model-facing tool name.
/// Provider wire APIs constrain tool names to `[A-Za-z0-9_-]{1,64}` (a name
/// like the plugin namespacing's `remora:remora-task` would make every
/// provider request of the session fail with a 400 — tool definitions ride
/// each request's cached prefix). Out-of-charset characters map to `_`.
/// `None` = unusable (empty after cleaning, or a collision with an
/// already-registered tool name) and the assembly skips the definition's
/// delegation tool loudly.
pub fn sanitized_tool_name(def_name: &str, taken: &HashSet<String>) -> Option<String> {
    let cleaned: String = def_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    if cleaned.is_empty() || taken.contains(&cleaned) {
        None
    } else {
        Some(cleaned)
    }
}

/// One definition's assembly result: the sanitized wire tool name and the
/// definition `tools:` allow list resolved (warn-and-skip) against the
/// child snapshot.
pub struct DelegationAssembly {
    pub tool_name: String,
    pub default_tools: Vec<String>,
}

/// Resolve one agent definition into its delegation tool identity.
/// `None` = the definition is skipped loudly (no usable wire name after
/// sanitizing, or a collision with an already-taken tool name). Definition
/// `tools:` data is definition data, not a call parameter: unknown names
/// and privileged names (human-interaction tools, `Steer`) warn and skip
/// instead of failing every dispatch — the request-param filter path stays
/// fail-loud.
pub fn resolve_delegation_tool(
    def: &manox_harness::ext_point_agent::AgentDef,
    taken: &HashSet<String>,
    snapshot_names: &HashSet<String>,
) -> Option<DelegationAssembly> {
    let tool_name = sanitized_tool_name(&def.name, taken)?;
    if tool_name != def.name {
        tracing::info!(
            def = %def.name,
            tool = %tool_name,
            "agent definition name sanitized into the provider wire charset"
        );
    }
    let default_tools: Vec<String> = def
        .tools
        .iter()
        .filter(|name| {
            if snapshot_names.contains(*name) {
                true
            } else if manox_harness::subagent::HUMAN_INTERACTION_TOOLS.contains(&name.as_str())
                || name.as_str() == "Steer"
            {
                tracing::warn!(
                    agent = %def.name,
                    tool = %name,
                    "agent definition names a tool children never receive; subagents never \
                     get it (DELEGATED_CALLER guard)"
                );
                false
            } else {
                tracing::warn!(
                    agent = %def.name,
                    tool = %name,
                    "agent definition names a tool not in the child snapshot; skipping it"
                );
                false
            }
        })
        .cloned()
        .collect();
    Some(DelegationAssembly {
        tool_name,
        default_tools,
    })
}

// ── DelegationTool ───────────────────────────────────────────────────────

/// One model-facing delegation tool, configured from a single agent
/// definition. The definition's body is the child's persona, its `tools`
/// frontmatter the default allow filter, and its `model` frontmatter the
/// default route (explicit per-call `model`/`reasoning_effort` parameters
/// win over the configured spec, which wins over the frontmatter).
pub struct DelegationTool {
    runtime: Arc<SubagentRuntime>,
    observer: Arc<SubagentRunObserver>,
    config: DelegationToolConfig,
    /// Registry for resolving model specs (`provider::model::effort`).
    /// Resolution happens at dispatch — the provider catalog may still be
    /// filling in when a resume-at-launch session assembles, and an
    /// unresolvable declared route fails that dispatch loudly rather than
    /// silently inheriting the caller's model.
    provider_registry: Option<Arc<manox_harness::core::ProviderRegistry>>,
    parent_depth: u32,
    parent_session: Option<String>,
    env: Arc<dyn manox_harness::core::env::ExecutionEnv>,
    cwd: PathBuf,
}

impl DelegationTool {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        runtime: Arc<SubagentRuntime>,
        observer: Arc<SubagentRunObserver>,
        config: DelegationToolConfig,
        provider_registry: Option<Arc<manox_harness::core::ProviderRegistry>>,
        parent_depth: u32,
        parent_session: Option<String>,
        env: Arc<dyn manox_harness::core::env::ExecutionEnv>,
        cwd: PathBuf,
    ) -> Self {
        DelegationTool {
            runtime,
            observer,
            config,
            provider_registry,
            parent_depth,
            parent_session,
            env,
            cwd,
        }
    }

    /// Resolve a declared spec (`config` wins over `frontmatter`) at
    /// dispatch time. `Ok(None)` when nothing is declared; a declared
    /// override that cannot resolve is a loud dispatch error — never a
    /// silent fallback to the caller's model.
    fn resolve_declared_model(
        &self,
    ) -> Result<Option<(manox_harness::types::Model, Option<String>)>, ToolError> {
        if let Some(spec) = self.config.config_model_spec.as_deref() {
            let registry = self.provider_registry.as_ref().ok_or_else(|| {
                ToolError::ExecutionFailed(format!(
                    "agent `{}` has a dedicated model config `{spec}` but no provider registry \
                     is available to resolve it",
                    self.config.name
                ))
            })?;
            let resolved =
                manox_harness::model_ref::resolve_model_spec(registry, spec).map_err(|e| {
                    ToolError::ExecutionFailed(format!(
                        "agent `{}` dedicated model config `{spec}` did not resolve: {e}",
                        self.config.name
                    ))
                })?;
            return Ok(Some((resolved.model, resolved.effort)));
        }
        if let Some(reference) = self.config.frontmatter_model_spec.as_deref() {
            let registry = self.provider_registry.as_ref().ok_or_else(|| {
                ToolError::ExecutionFailed(format!(
                    "agent `{}` declares model override `{reference}` but no provider registry \
                     is available to resolve it",
                    self.config.name
                ))
            })?;
            let model = manox_harness::model_ref::resolve_model_ref(registry, reference)
                .ok_or_else(|| {
                    ToolError::ExecutionFailed(format!(
                        "agent `{}` model override `{reference}` did not resolve",
                        self.config.name
                    ))
                })?;
            return Ok(Some((model, None)));
        }
        Ok(None)
    }
}

/// Build the model-facing description: the definition's own description and
/// capability tag plus the dispatch contract (fresh context, foreground by
/// default, background semantics, budgets). Model-facing English — never
/// localized.
pub fn delegation_description(description: &str, capability: &str, name: &str) -> String {
    format!(
        "{description} [capability: {capability}] Dispatch a `{name}` subagent — a \
         fresh-context worker that cannot see this conversation. Required: \
         `description` (short task label) and `prompt` (the full task; the child \
         only knows what you write). By default the call waits and returns the \
         child's final report; set `run_in_background: true` to run it on a \
         background card instead and receive the report as a message when it \
         settles. `timeout_ms`/`idle_timeout_ms` (min {}) arm kill budgets that \
         deliver a partial report on expiry. A child's approval-requiring \
         operations are auto-rejected; it cannot delegate further.",
        manox_harness::subagent::MIN_BUDGET_MS
    )
}

#[async_trait::async_trait]
impl AgentTool for DelegationTool {
    fn name(&self) -> &str {
        &self.config.name
    }

    fn description(&self) -> &str {
        &self.config.rendered_description
    }

    fn is_read_only(&self) -> bool {
        self.config.capability == "read-only"
    }

    fn requires_approval(&self, _params: &serde_json::Value) -> bool {
        false
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "description": {
                    "type": "string",
                    "description": "Short task label (shown on the run card and used as the child's creation label)."
                },
                "prompt": {
                    "type": "string",
                    "description": "The full task for the child. It runs with a fresh context: include every fact, path, and done-criterion it needs, and ask for a concise final report."
                },
                "model": {
                    "type": "string",
                    "description": "Optional model override (provider::model, or provider::model::effort)."
                },
                "reasoning_effort": {
                    "type": "string",
                    "enum": ["off", "low", "medium", "high", "max"],
                    "description": "Optional reasoning effort for the child's model."
                },
                "run_in_background": {
                    "type": "boolean",
                    "description": "Run on a background card and deliver the report as a message on settlement instead of blocking this call until the final report."
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "Optional wall-clock budget in milliseconds (min 1000). On expiry the child is terminated and a report with its partial output is delivered."
                },
                "idle_timeout_ms": {
                    "type": "integer",
                    "description": "Optional idle budget in milliseconds (min 1000): the longest stretch with no activity and no tool running before the child is terminated as stalled. Enforcement granularity is ~5s."
                },
                "isolation": {
                    "type": "string",
                    "enum": ["worktree"],
                    "description": "Run the child in a throwaway git worktree; a worktree with committed work is kept and reported."
                }
            },
            "required": ["description", "prompt"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        signal: CancellationToken,
        _ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        let description = string_param(&params, "description")
            .ok_or_else(|| ToolError::InvalidArguments("'description' is required".into()))?;
        let prompt = string_param(&params, "prompt")
            .ok_or_else(|| ToolError::InvalidArguments("'prompt' is required".into()))?;
        let run_in_background = params
            .get("run_in_background")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let budgets = Budgets {
            timeout_ms: params.get("timeout_ms").and_then(|v| v.as_u64()),
            idle_timeout_ms: params.get("idle_timeout_ms").and_then(|v| v.as_u64()),
        };
        budgets
            .validate()
            .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
        let isolation = params.get("isolation").and_then(|v| v.as_str());
        if self.parent_depth + 1 > self.config.max_depth {
            return Err(ToolError::InvalidArguments(format!(
                "delegation depth budget exhausted: this agent may not delegate (cap {})",
                self.config.max_depth
            )));
        }
        let mut tool_filter: Option<ToolFilter> = None;
        if !self.config.default_tools.is_empty() {
            tool_filter = Some(ToolFilter {
                allow: self.config.default_tools.clone(),
                deny: Vec::new(),
            });
        }
        // Model precedence: explicit parameter > dedicated config spec >
        // frontmatter override. A parameter naming a model resolves it; an
        // unresolvable name fails the dispatch loudly.
        let declared = self.resolve_declared_model()?;
        let (model, declared_effort) = match string_param(&params, "model") {
            Some(spec) => {
                let registry = self.provider_registry.as_ref().ok_or_else(|| {
                    ToolError::InvalidArguments(
                        "no provider registry is available to resolve the requested model".into(),
                    )
                })?;
                let resolved = manox_harness::model_ref::resolve_model_spec(registry, &spec)
                    .map_err(|e| {
                        ToolError::InvalidArguments(format!("model `{spec}` did not resolve: {e}"))
                    })?;
                (Some(resolved.model), resolved.effort)
            }
            None => match declared {
                Some((model, effort)) => (Some(model), effort),
                None => (None, None),
            },
        };
        let effort = match string_param(&params, "reasoning_effort") {
            Some(requested) => Some(requested),
            None => declared_effort,
        };
        let agent_options = if model.is_some() || effort.is_some() {
            Some(manox_harness::subagent::AgentOptions {
                model,
                reasoning_effort: effort,
            })
        } else {
            None
        };
        let card_title = description.clone();
        let request = manox_harness::subagent::StartRequest {
            label: description,
            kind: self.config.name.clone(),
            prompt,
            agent_options,
            max_depth: Some(self.config.max_depth),
            tool_filter,
            persona: Some(self.config.persona.clone()),
            parent_depth: self.parent_depth,
            parent_session: self.parent_session.clone(),
            budgets,
            isolation: isolation.map(String::from),
            env: Arc::clone(&self.env),
            cwd: self.cwd.clone(),
            // A background run's cancellation is its own: the dispatching
            // turn's abort must not reach it (it survives to deliver its
            // report), and cancellation belongs to InterruptAgent / the
            // card's TaskStop — both drive the runtime's dispose token.
            cancel: if run_in_background {
                CancellationToken::new()
            } else {
                signal.clone()
            },
        };
        let run = self
            .runtime
            .start(&self.config.provider, request)
            .await
            .map_err(tool_error_of)?;
        let run_id = run.id.clone();
        let run_id_for_result = run_id.clone();
        if !run_in_background {
            let result = run.result().await;
            self.observer.retire(&run_id);
            if result.stop_reason == StopReason::Aborted && signal.is_cancelled() {
                return Err(ToolError::ExecutionFailed("canceled".into()));
            }
            return Ok(foreground_result(&run_id_for_result, result));
        }
        // Background: card + peer delivery of the settled report. The card
        // title is the dispatch's `description` (the run label), not a
        // watchdog activity line.
        let cancel = run.dispose_token().clone();
        let (task_id, task) = background_task::register(
            TaskKind::Subagent,
            self.owner_thread_id(),
            card_title,
            cancel,
        );
        let _ = self.notice_tx().send(BackendNotice::Event(Box::new(
            ThreadEvent::BackgroundTaskUpdated {
                snapshot: task.snapshot(&task_id),
            },
        )));
        let runtime = Arc::clone(&self.runtime);
        let observer = Arc::clone(&self.observer);
        let notice_tx = self.notice_tx();
        let run_id_for_delivery = run_id.clone();
        tokio::spawn(async move {
            let _keep_runtime = runtime; // the run table outlives the tool call
            let result = run.result().await;
            let stats = observer.stats_of(&run_id_for_delivery);
            let delivery = background_delivery(&result, stats);
            observer.retire(&run_id_for_delivery);
            match result.stop_reason {
                StopReason::Aborted => task.set_terminal_status(TaskStatus::Stopped),
                StopReason::Completed => task.set_terminal_status(TaskStatus::Completed),
                _ => {
                    task.set_failure_summary(
                        result
                            .diagnostic
                            .clone()
                            .unwrap_or_else(|| "failed".to_string()),
                    );
                    task.set_terminal_status(TaskStatus::Failed);
                }
            }
            let _ = notice_tx.send(BackendNotice::Event(Box::new(
                ThreadEvent::BackgroundTaskUpdated {
                    snapshot: task.snapshot(&task_id),
                },
            )));
            if let Some(text) = delivery {
                let _ = notice_tx.send(BackendNotice::SteerDelivered {
                    from: manox_harness::steer_bus::AgentId::Subagent(run_id_for_delivery),
                    reason: manox_harness::steer_bus::SteerReason::Complete,
                    payload: manox_harness::steer_bus::SteerPayload { text },
                });
            }
            let _ = observer; // rail settlement already happened via on_settled
        });
        Ok(AgentToolResult::text(
            serde_json::json!({
                "kind": "background",
                "run_id": run_id,
                "status": "started",
            })
            .to_string(),
        ))
    }
}

impl DelegationTool {
    fn owner_thread_id(&self) -> String {
        self.observer.owner_thread_id.clone()
    }
    fn notice_tx(&self) -> mpsc::UnboundedSender<BackendNotice> {
        self.observer.notice_tx.clone()
    }
}

fn string_param(params: &serde_json::Value, key: &str) -> Option<String> {
    params.get(key).and_then(|v| v.as_str()).map(str::to_string)
}

fn tool_error_of(error: manox_harness::subagent::SubagentError) -> ToolError {
    match error {
        manox_harness::subagent::SubagentError::InvalidRequest(_)
        | manox_harness::subagent::SubagentError::UnsupportedCapability { .. }
        | manox_harness::subagent::SubagentError::UnknownProvider(_) => {
            ToolError::InvalidArguments(error.to_string())
        }
        manox_harness::subagent::SubagentError::Provider(_, _) => {
            ToolError::ExecutionFailed(error.to_string())
        }
    }
}

/// The foreground tool result: a JSON envelope with the run id (restore
/// keys the rail row off it), the stop reason, and the output — an error
/// result when the run did not complete, with the diagnostic and the
/// salvaged partial output.
fn foreground_result(run_id: &str, result: RunResult) -> AgentToolResult {
    let completed = result.stop_reason == StopReason::Completed;
    let mut envelope = serde_json::json!({
        "kind": "foreground",
        "run_id": run_id,
        "stop_reason": result.stop_reason.to_string(),
        "output": result.output,
    });
    if let Some(structured) = &result.structured {
        envelope["structured"] = structured.clone();
    }
    if let Some(diagnostic) = &result.diagnostic {
        envelope["error"] = serde_json::json!(diagnostic);
    }
    let mut tool_result = AgentToolResult::text(envelope.to_string());
    tool_result.is_error = !completed;
    tool_result
}

/// The background peer-delivery text: the final report on completion; on a
/// delivering failure the diagnostic (already carrying the shared
/// failure/timed-out prefixes restore keys on) plus the watchdog's run
/// counters and whatever partial output the child had produced; `None` on
/// an explicit abort (the parent asked for it — no report revives the
/// turn).
fn background_delivery(result: &RunResult, stats: Option<(u64, u64)>) -> Option<String> {
    let footer = stats
        .map(|(turns, tool_calls)| format!(" ({turns} turns, {tool_calls} tool calls)"))
        .unwrap_or_default();
    match result.stop_reason {
        StopReason::Completed => {
            if result.output.is_empty() {
                Some(format!("completed with no output{footer}"))
            } else {
                Some(result.output.clone())
            }
        }
        StopReason::Aborted => None,
        _ => {
            let diagnostic = result
                .diagnostic
                .clone()
                .unwrap_or_else(|| "subagent failed".to_string());
            if result.output.is_empty() {
                Some(format!("{diagnostic}{footer}"))
            } else {
                Some(format!(
                    "{diagnostic}{footer}\n\nPartial result:\n{}",
                    result.output
                ))
            }
        }
    }
}

// ── Control tools ────────────────────────────────────────────────────────

/// The `ListAgents` control tool: the live-run table snapshot with the
/// watchdog's health verdict per row (the dsh `list_agents`, children
/// scope; one-shot runs are always direct children).
pub struct ListAgentsTool {
    runtime: Arc<SubagentRuntime>,
    observer: Arc<SubagentRunObserver>,
}

impl ListAgentsTool {
    pub const NAME: &str = "ListAgents";

    pub fn new(runtime: Arc<SubagentRuntime>, observer: Arc<SubagentRunObserver>) -> Self {
        ListAgentsTool { runtime, observer }
    }
}

#[async_trait::async_trait]
impl AgentTool for ListAgentsTool {
    fn name(&self) -> &str {
        "ListAgents"
    }

    fn description(&self) -> &str {
        "List this thread's live subagent runs: id, task label, provider, \
         health, and running time. Check it when a background run seems slow \
         or silent before deciding to InterruptAgent or re-dispatch — do not \
         guess."
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn requires_approval(&self, _params: &serde_json::Value) -> bool {
        false
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {}
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        _params: serde_json::Value,
        _signal: CancellationToken,
        _ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        let mut rows: Vec<serde_json::Value> = self
            .runtime
            .list_runs()
            .into_iter()
            .map(|run| {
                let mut row = serde_json::json!({
                    "id": run.id,
                    "kind": run.kind,
                    "label": run.label,
                    "provider": run.provider,
                    "status": run.status,
                    "running_for_ms": run.running_for_ms,
                });
                if let Some(health) = self.observer.health_of(&run.id) {
                    row["health"] = serde_json::json!(health);
                }
                if let Some((turns, tool_calls)) = self.observer.stats_of(&run.id) {
                    row["turns"] = serde_json::json!(turns);
                    row["tool_calls"] = serde_json::json!(tool_calls);
                }
                row
            })
            .collect();
        rows.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
        if rows.is_empty() {
            return Ok(AgentToolResult::text("no live agents".to_string()));
        }
        Ok(AgentToolResult::text(
            serde_json::to_string_pretty(&rows)
                .unwrap_or_else(|_| "agent list unavailable".to_string()),
        ))
    }
}

/// The `InterruptAgent` control tool: cancel one live run (the dsh
/// `interrupt_agent`, parent-adjacency enforced structurally — this runtime
/// only tracks this thread's children).
pub struct InterruptAgentTool {
    runtime: Arc<SubagentRuntime>,
}

impl InterruptAgentTool {
    pub const NAME: &str = "InterruptAgent";

    pub fn new(runtime: Arc<SubagentRuntime>) -> Self {
        InterruptAgentTool { runtime }
    }
}

#[async_trait::async_trait]
impl AgentTool for InterruptAgentTool {
    fn name(&self) -> &str {
        "InterruptAgent"
    }

    fn description(&self) -> &str {
        "Cancel one of this thread's live subagent runs by id (as reported \
         by ListAgents or the dispatch result). The run settles silently — \
         no report is delivered for an interruption you requested."
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn requires_approval(&self, _params: &serde_json::Value) -> bool {
        false
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "agent_id": {
                    "type": "string",
                    "description": "The run id (e.g. \"sub-0\")."
                }
            },
            "required": ["agent_id"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        _signal: CancellationToken,
        _ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        let agent_id = params
            .get("agent_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArguments("'agent_id' is required".into()))?;
        if self.runtime.interrupt(agent_id) {
            Ok(AgentToolResult::text(
                serde_json::json!({"interrupted": true, "agent_id": agent_id}).to_string(),
            ))
        } else {
            Err(ToolError::InvalidArguments(format!(
                "no live agent {agent_id}"
            )))
        }
    }
}

// ── RunObserver bridge ───────────────────────────────────────────────────

/// One live run's observation state: the watchdog the run loop feeds via
/// `on_child_event`/`on_tick`, and the definition name the progress rows
/// carry.
struct RunWatch {
    subagent_type: String,
    watchdog: Arc<Mutex<SubagentWatchdog>>,
}

/// The delivery inputs of a settled run, kept after the live row is
/// removed: a tool future dropped by turn teardown never reaches its
/// `retire`, and the settling delivery still needs the counters. Entries
/// are bounded by the run count of one assembly and retire with the run.
struct SettledStats {
    turns: u64,
    tool_calls: u64,
}

/// The host's observer: folds ext-level run events into the transcript rail
/// (`SubagentProgress`/`SubagentChild`), drives the per-run health
/// watchdog, and remembers the first activity per run for card titles.
pub struct SubagentRunObserver {
    pub(crate) owner_thread_id: String,
    pub(crate) notice_tx: mpsc::UnboundedSender<BackendNotice>,
    runs: Mutex<BTreeMap<String, RunWatch>>,
    settled: Mutex<BTreeMap<String, SettledStats>>,
}

impl SubagentRunObserver {
    pub fn new(
        owner_thread_id: String,
        notice_tx: mpsc::UnboundedSender<BackendNotice>,
    ) -> Arc<Self> {
        Arc::new(SubagentRunObserver {
            owner_thread_id,
            notice_tx,
            runs: Mutex::new(BTreeMap::new()),
            settled: Mutex::new(BTreeMap::new()),
        })
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, RunWatch>> {
        self.runs.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The watchdog's current health line for one run, for `ListAgents`.
    pub fn health_of(&self, run_id: &str) -> Option<String> {
        let now = std::time::Instant::now();
        let runs = self.locked();
        runs.get(run_id).map(|watch| {
            watch
                .watchdog
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .health_line(now)
        })
    }

    /// The watchdog's run counters for one run — `(turns, tool_calls)` —
    /// for the background delivery footer. Live rows answer from the
    /// watchdog; settled runs from the retained settled table (so a tool
    /// future dropped by turn teardown still settles its delivery).
    pub fn stats_of(&self, run_id: &str) -> Option<(u64, u64)> {
        if let Some(watch) = self.locked().get(run_id) {
            let wd = watch.watchdog.lock().unwrap_or_else(|e| e.into_inner());
            return Some((wd.turns(), wd.tool_calls()));
        }
        self.settled
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(run_id)
            .map(|s| (s.turns, s.tool_calls))
    }

    /// Drop one run's observation state (live row and settled entry). The
    /// tool calls this after reading what it needs from the settled run
    /// (health → delivery footers).
    pub fn retire(&self, run_id: &str) {
        self.locked().remove(run_id);
        self.settled
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(run_id);
    }

    fn emit_progress(
        &self,
        run_id: &str,
        subagent_type: &str,
        activity: Option<String>,
        status: crate::thread::ToolCallStatus,
        health: Option<String>,
    ) {
        let _ = self.notice_tx.send(BackendNotice::Event(Box::new(
            ThreadEvent::SubagentProgress {
                id: run_id.to_string(),
                subagent_type: subagent_type.to_string(),
                tool_uses: 0,
                token_usage: crate::language_model::TokenUsage::default(),
                latest_activity: activity,
                status,
                health,
            },
        )));
    }
}

impl RunObserver for SubagentRunObserver {
    fn on_start(&self, info: &RunInfo) {
        let subagent_type = info.kind.clone();
        self.locked().insert(
            info.run_id.clone(),
            RunWatch {
                subagent_type: subagent_type.clone(),
                watchdog: Arc::new(Mutex::new(SubagentWatchdog::new(
                    std::time::Instant::now(),
                    info.budgets.idle_timeout_ms,
                ))),
            },
        );
        if let Some(model) = &info.model {
            let _ =
                self.notice_tx
                    .send(BackendNotice::Event(Box::new(ThreadEvent::SubagentChild {
                        id: info.run_id.clone(),
                        child: crate::thread::SubagentChildEvent::Model(
                            crate::provider_glue::display_name(model),
                        ),
                    })));
        }
        self.emit_progress(
            &info.run_id,
            &subagent_type,
            Some(info.label.clone()),
            crate::thread::ToolCallStatus::Running,
            Some("starting".into()),
        );
    }

    fn on_child_event(&self, run_id: &str, event: &manox_harness::types::AgentEvent) {
        // Health folds first: a state change publishes one throttled health
        // line; then the transcript activity rides as before.
        let health_change = {
            let runs = self.locked();
            match runs.get(run_id) {
                Some(watch) => {
                    let mut wd = watch.watchdog.lock().unwrap_or_else(|e| e.into_inner());
                    let now = std::time::Instant::now();
                    wd.observe(event, now).then(|| wd.health_line(now))
                }
                None => None,
            }
        };
        if let Some(line) = health_change {
            let subagent_type = {
                let runs = self.locked();
                runs.get(run_id)
                    .map(|w| w.subagent_type.clone())
                    .unwrap_or_default()
            };
            self.emit_progress(
                run_id,
                &subagent_type,
                None,
                crate::thread::ToolCallStatus::Running,
                Some(line),
            );
        }
        for ev in crate::engine::adapt::child_events_of(run_id, event) {
            let _ = self.notice_tx.send(BackendNotice::Event(Box::new(ev)));
        }
    }

    fn on_tick(&self, run_id: &str) -> bool {
        let outcome = {
            let runs = self.locked();
            let Some(watch) = runs.get(run_id) else {
                return false;
            };
            let mut wd = watch.watchdog.lock().unwrap_or_else(|e| e.into_inner());
            wd.tick(std::time::Instant::now())
        };
        match outcome {
            crate::subagent_watchdog::TickOutcome::Idle => false,
            crate::subagent_watchdog::TickOutcome::ReportStall => {
                let (subagent_type, line) = {
                    let runs = self.locked();
                    match runs.get(run_id) {
                        Some(watch) => {
                            let wd = watch.watchdog.lock().unwrap_or_else(|e| e.into_inner());
                            (
                                watch.subagent_type.clone(),
                                wd.health_line(std::time::Instant::now()),
                            )
                        }
                        None => return false,
                    }
                };
                self.emit_progress(
                    run_id,
                    &subagent_type,
                    None,
                    crate::thread::ToolCallStatus::Running,
                    Some(line),
                );
                false
            }
            crate::subagent_watchdog::TickOutcome::EnforceStall => true,
        }
    }

    fn on_settled(&self, end: &RunEndInfo) {
        // The live row retires here; its delivery inputs move into the
        // settled table so the settling tool (or a delivery task whose
        // sibling tool future was dropped by turn teardown) can still read
        // them. `retire` clears the table entry.
        let (subagent_type, stats) = {
            let mut runs = self.locked();
            match runs.remove(&end.run_id) {
                Some(watch) => {
                    let (turns, tool_calls) = {
                        let wd = watch.watchdog.lock().unwrap_or_else(|e| e.into_inner());
                        (wd.turns(), wd.tool_calls())
                    };
                    (watch.subagent_type, Some((turns, tool_calls)))
                }
                None => (end.label.clone(), None),
            }
        };
        if let Some((turns, tool_calls)) = stats {
            self.settled
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(end.run_id.clone(), SettledStats { turns, tool_calls });
        }
        let (status, activity) = match end.stop_reason {
            StopReason::Completed => (
                crate::thread::ToolCallStatus::Success,
                Some(end.output.clone()),
            ),
            StopReason::Aborted => (
                crate::thread::ToolCallStatus::Cancelled,
                Some("aborted".into()),
            ),
            ref reason => (
                crate::thread::ToolCallStatus::Error,
                Some(format!("failed: {reason}")),
            ),
        };
        self.emit_progress(&end.run_id, &subagent_type, activity, status, None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(tool_result: &AgentToolResult) -> String {
        match &tool_result.content[0] {
            manox_harness::types::ContentBlock::Text { text, .. } => text.clone(),
            other => panic!("expected a text block: {other:?}"),
        }
    }

    fn result(stop_reason: StopReason, output: &str, diagnostic: Option<&str>) -> RunResult {
        RunResult {
            output: output.to_string(),
            structured: None,
            diagnostic: diagnostic.map(str::to_string),
            stop_reason,
        }
    }

    #[test]
    fn foreground_envelope_carries_run_id_stop_reason_and_output() {
        let tool_result =
            foreground_result("sub-7", result(StopReason::Completed, "the answer", None));
        assert!(!tool_result.is_error);
        let envelope: serde_json::Value = serde_json::from_str(&text_of(&tool_result)).unwrap();
        assert_eq!(envelope["kind"], "foreground");
        assert_eq!(envelope["run_id"], "sub-7");
        assert_eq!(envelope["stop_reason"], "completed");
        assert_eq!(envelope["output"], "the answer");
    }

    #[test]
    fn foreground_failure_is_an_error_result_with_diagnostic_and_partial() {
        let tool_result = foreground_result(
            "sub-8",
            result(
                StopReason::Error,
                "partial work",
                Some("subagent timed out: budget 1000ms exceeded"),
            ),
        );
        assert!(tool_result.is_error);
        let envelope: serde_json::Value = serde_json::from_str(&text_of(&tool_result)).unwrap();
        assert_eq!(envelope["stop_reason"], "error");
        assert_eq!(envelope["output"], "partial work");
        assert!(
            envelope["error"]
                .as_str()
                .unwrap()
                .starts_with("subagent timed out")
        );
    }

    /// Completed background runs deliver the final report; failures deliver
    /// the diagnostic (already carrying the shared prefixes restore keys
    /// on); an explicit abort delivers nothing (the parent asked for it).
    #[test]
    fn background_delivery_mapping() {
        let completed =
            background_delivery(&result(StopReason::Completed, "final report", None), None);
        assert_eq!(completed.as_deref(), Some("final report"));

        let failed = background_delivery(
            &result(StopReason::Error, "partial", Some("subagent failed: boom")),
            Some((2, 5)),
        );
        let failed = failed.unwrap();
        assert!(
            failed.starts_with(SUBAGENT_FAILED_DELIVERY_PREFIX),
            "{failed}"
        );
        assert!(failed.contains("(2 turns, 5 tool calls)"), "{failed}");
        assert!(failed.contains("Partial result:\npartial"), "{failed}");

        let timed_out = background_delivery(
            &result(
                StopReason::Error,
                "",
                Some("subagent timed out: budget 1000ms exceeded"),
            ),
            None,
        );
        let timed_out = timed_out.unwrap();
        assert!(timed_out.starts_with(SUBAGENT_TIMED_OUT_DELIVERY_PREFIX));
        assert!(!timed_out.contains("Partial result"), "{timed_out}");

        assert_eq!(
            background_delivery(&result(StopReason::Aborted, "", None), None),
            None
        );
    }

    /// Assembly-level resolution: a plugin-namespaced def sanitizes into a
    /// safe wire name, a collision skips the def, and definition `tools:`
    /// data resolves warn-and-skip against the child snapshot (unknown and
    /// privileged names dropped, known names kept).
    #[test]
    fn resolve_delegation_tool_sanitizes_skips_and_warns() {
        let snapshot: HashSet<String> = ["Read", "Grep"].iter().map(|s| s.to_string()).collect();
        let mut def = manox_harness::ext_point_agent::AgentDef {
            name: "remora:remora-task".into(),
            description: "d".into(),
            tools: vec!["Read".into(), "AskUserQuestion".into(), "Ghost".into()],
            model: None,
            system_prompt: "p".into(),
        };
        let assembly = crate::subagent::resolve_delegation_tool(&def, &HashSet::new(), &snapshot)
            .expect("a namespaced def resolves");
        assert_eq!(assembly.tool_name, "remora_remora-task");
        assert_eq!(
            assembly.default_tools,
            vec!["Read".to_string()],
            "privileged (AskUserQuestion) and unknown (Ghost) names warn-and-skip"
        );

        // A collision with an already-taken wire name skips the def.
        let taken: HashSet<String> = ["remora_remora-task".to_string()].into_iter().collect();
        assert!(crate::subagent::resolve_delegation_tool(&def, &taken, &snapshot).is_none());

        // A collision between two defs resolves the second only after the
        // first's name is taken into account — the engine inserts as it
        // goes, so distinct defs never collapse onto one tool.
        def.name = "remora.task".into();
        let assembly = crate::subagent::resolve_delegation_tool(&def, &taken, &snapshot)
            .expect("a distinct def still resolves");
        assert_eq!(assembly.tool_name, "remora_task");
    }

    /// Plugin namespacing (`remora:remora-task`) must never reach the wire:
    /// `:` sanitizes to `_`; a collision or empty result skips the def.
    #[test]
    fn sanitized_tool_name_maps_wire_charset_and_rejects_collisions() {
        let taken: HashSet<String> = ["Sailor", "remora_remora-task"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            crate::subagent::sanitized_tool_name("remora:remora-task", &taken),
            None,
            "collision with an existing tool name is skipped"
        );
        assert_eq!(
            crate::subagent::sanitized_tool_name("remora:remora-task", &HashSet::new()),
            Some("remora_remora-task".to_string())
        );
        assert_eq!(
            crate::subagent::sanitized_tool_name("Explore", &HashSet::new()),
            Some("Explore".to_string()),
            "charset-clean names pass through"
        );
        let long = "x".repeat(70);
        assert_eq!(
            crate::subagent::sanitized_tool_name(&long, &HashSet::new())
                .unwrap()
                .chars()
                .count(),
            64
        );
    }

    #[test]
    fn failed_delivery_prefix_constant_is_shared_with_restore() {
        assert_eq!(SUBAGENT_FAILED_DELIVERY_PREFIX, "subagent failed: ");
        assert_eq!(SUBAGENT_TIMED_OUT_DELIVERY_PREFIX, "subagent timed out: ");
    }
}
