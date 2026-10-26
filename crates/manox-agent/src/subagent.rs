//! Host-side subagent delegation surface:
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
use std::time::Duration;

use manox_harness::subagent::{
    Budgets, DelegationToolConfig, RunEndInfo, RunInfo, RunObserver, RunResult, StopReason,
    SubagentRuntime, ToolFilter,
};
use manox_harness::tool::{AgentTool, AgentToolResult, ToolContext, ToolError};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::background_task;
use crate::subagent_watchdog::SubagentWatchdog;
use crate::thread::ThreadEvent;
use crate::thread_engine::{BackendNotice, send_notice};

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
        // Continue a settled run: the child session the id names reopens and
        // `prompt` is the follow-up. The dispatch otherwise behaves exactly
        // like a first run — same budgets, promotion, settlement, delivery.
        let continue_run_id = string_param(&params, "continue_run_id");
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
        // Foreground waiting is a choice with an exit: the wall-clock budget
        // promotes the wait instead of killing the run. A foreground run is
        // therefore started WITHOUT the wall-clock budget (the idle watchdog
        // stays armed) and the caller enforces promotion itself — the run
        // never races a kill it could have survived.
        let promote_after = if run_in_background {
            None
        } else {
            budgets.timeout_ms
        };
        let request = manox_harness::subagent::StartRequest {
            label: description,
            kind: self.config.name.clone(),
            prompt: prompt.clone(),
            agent_options,
            max_depth: Some(self.config.max_depth),
            tool_filter,
            persona: Some(self.config.persona.clone()),
            parent_depth: self.parent_depth,
            parent_session: self.parent_session.clone(),
            tool_call_id: Some(_tool_call_id.to_string()),
            continuation_of: continue_run_id.clone(),
            budgets: if run_in_background {
                budgets
            } else {
                manox_harness::subagent::Budgets {
                    timeout_ms: None,
                    idle_timeout_ms: budgets.idle_timeout_ms,
                }
            },
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
        let run = match &continue_run_id {
            Some(run_id) => self
                .runtime
                .continue_run(&self.config.provider, run_id, request)
                .await
                .map_err(tool_error_of)?,
            None => self
                .runtime
                .start(&self.config.provider, request)
                .await
                .map_err(tool_error_of)?,
        };
        let run_id = run.id.clone();
        let run_id_for_result = run_id.clone();
        // The run.s lifecycle row is the durable record — two rows per run,
        // written once each; progress never rides the parent journal.
        match &continue_run_id {
            Some(_) => self.observer.journal_swarm_row(
                "run/continued",
                serde_json::json!({
                    "runId": run_id.clone(),
                    "followUp": prompt,
                }),
            ),
            None => self.observer.journal_swarm_row(
                "run/started",
                serde_json::json!({
                    "runId": run_id.clone(),
                    "kind": self.config.name,
                    "label": card_title.clone(), // the delivery keeps the label
                    "childSession": run_id.clone(), // the invariant: the run id names the child session
                    "toolCallId": _tool_call_id,
                    "budgets": {
                        "timeoutMs": budgets.timeout_ms,
                        "idleTimeoutMs": budgets.idle_timeout_ms,
                    },
                    "depth": self.parent_depth + 1,
                }),
            ),
        }
        if !run_in_background {
            let settled = match promote_after {
                None => run.result().await,
                Some(ms) => tokio::select! {
                    r = run.result() => r,
                    _ = tokio::time::sleep(std::time::Duration::from_millis(ms)) => {
                        // Promotion: the tool call answers now, the run keeps
                        // going under its idle watchdog, and the report
                        // arrives through the same settlement delivery as a
                        // dispatched background run.
                        let cancel = run.dispose_token().clone();
                        let (task_id, task) =
                            background_task::register(self.owner_thread_id(), card_title.clone(), cancel);
                        send_notice(
                            &self.notice_tx(),
                            BackendNotice::Event(Box::new(ThreadEvent::BackgroundTaskUpdated {
                                snapshot: task.snapshot(&task_id),
                            })),
                            "promoted subagent task card",
                        );
                        spawn_settlement_delivery(
                            Arc::clone(&self.runtime),
                            Arc::clone(&self.observer),
                            run,
                            run_id_for_result.clone(),
                            card_title,
                            task,
                            task_id,
                            // The promoted run was started WITHOUT a
                            // wall-clock budget (the dispatch's `timeout_ms`
                            // became this promotion timer). No budget, no
                            // net: a budgetless run cannot be distinguished
                            // from a legitimate long run.
                            None,
                        );
                        return Ok(AgentToolResult::text(
                            serde_json::json!({
                                "kind": "foreground",
                                "run_id": run_id_for_result,
                                "stop_reason": "promoted",
                                "output": format!(
                                    "promoted to background after {ms}ms; the report will arrive as a notification"
                                ),
                            })
                            .to_string(),
                        ));
                    }
                },
            };
            self.observer.retire(&run_id);
            if settled.stop_reason == StopReason::Aborted && signal.is_cancelled() {
                return Err(ToolError::ExecutionFailed("canceled".into()));
            }
            return Ok(foreground_result(&run_id_for_result, settled));
        }
        // Background: card + settlement delivery of the settled report.
        // The card title is the dispatch's `description` (the run label),
        // not a watchdog activity line.
        let cancel = run.dispose_token().clone();
        let (task_id, task) =
            background_task::register(self.owner_thread_id(), card_title.clone(), cancel);
        send_notice(
            &self.notice_tx(),
            BackendNotice::Event(Box::new(ThreadEvent::BackgroundTaskUpdated {
                snapshot: task.snapshot(&task_id),
            })),
            "subagent task card snapshot",
        );
        spawn_settlement_delivery(
            Arc::clone(&self.runtime),
            Arc::clone(&self.observer),
            run,
            run_id.clone(),
            card_title,
            task,
            task_id,
            // Net past budget + grace: the drive loop's own deadline settles
            // the run at the budget point, so a run whose result is still
            // absent a fixed grace period later has lost its settlement
            // chain. `settle_with_net` caps the armed duration.
            budgets
                .timeout_ms
                .map(|ms| Duration::from_millis(ms).saturating_add(SETTLEMENT_NET_GRACE)),
        );

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

/// Grace past the run's wall-clock budget before the settlement net fires.
/// The drive loop's own deadline settles the run at the budget point, so a
/// fixed 60s of additional silence means the settlement chain was lost.
const SETTLEMENT_NET_GRACE: Duration = Duration::from_secs(60);

/// Upper bound on the armed net. The wall-clock budget is caller-supplied
/// milliseconds; without a cap an absurd budget would keep a lost run's
/// capacity slot occupied for centuries and push `budget + grace` toward
/// `Duration` overflow. Thirty days is far past any legitimate delegation.
const SETTLEMENT_NET_MAX: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// The one sentence the net's settlement carries: the delivery footer's
/// diagnostic, and the durable row's salvage (so the `stalled (no activity)`
/// verdict is not left claiming a silence that did not happen).
const SETTLEMENT_NET_REASON: &str = "no terminal result within the run's wall-clock budget; \
     settled by the supervisor's settlement net";

/// Race the run's result against the settlement net. `net: None` = the run
/// carries no wall-clock budget: with no bound there is no telling a lost
/// settlement chain from a legitimate long run, so no net is armed and the
/// result is awaited bare. When the net fires, `on_net` runs first (the
/// caller cancels the run's residual child work and releases its capacity
/// slot) and the synthetic `Stalled` settlement — the same vocabulary as the
/// watchdog's forced stall — stands in for the result that never came.
/// Returns whether the net (not the result) produced the outcome.
///
/// The net covers exactly one failure mode: the delivery task and the drive
/// task are both alive and the result is never produced. A dead or panicking
/// drive synthesizes its own settlement at the source, and a wedge that
/// holds the observer's own lock is unreachable from here by construction.
async fn settle_with_net(
    result: impl std::future::Future<Output = RunResult>,
    net: Option<Duration>,
    on_net: impl FnOnce(),
) -> (RunResult, bool) {
    let Some(net) = net else {
        return (result.await, false);
    };
    let net = net.min(SETTLEMENT_NET_MAX);
    tokio::pin!(result);
    tokio::select! {
        result = result.as_mut() => (result, false),
        () = tokio::time::sleep(net) => {
            // A result that lands in the same instant the net expires is a
            // real settlement, not a lost one — re-check before synthesizing
            // (select! picks a ready branch at random, the same reason the
            // drive loop re-polls its prompt at its own deadline).
            if let std::task::Poll::Ready(result) = futures::poll!(result.as_mut()) {
                return (result, false);
            }
            on_net();
            (
                RunResult {
                    output: String::new(),
                    structured: None,
                    diagnostic: Some(SETTLEMENT_NET_REASON.to_string()),
                    stop_reason: StopReason::Stalled,
                },
                true,
            )
        }
    }
}

/// The one settlement delivery. Every terminal — completion, error,
/// timeout, stall, abort — lands the card settlement and delivers text
/// composed the same way: the host's verdict line, the salvaged output (or
/// the explicit no-closing-message line), the run counters, and any
/// diagnostic. Silence is never a delivery.
///
/// `net` (the run's wall-clock budget plus [`SETTLEMENT_NET_GRACE`]) races
/// the result. The drive loop settles at the budget point, so a run whose
/// result is still absent past the grace window has lost its settlement
/// chain: both tasks are alive, yet no terminal result will ever arrive (a
/// dead or panicking drive already synthesizes its own result, and a wedge
/// that holds the observer's own lock is not recoverable from inside this
/// task either). The net lands the durable settlement through this same
/// funnel.
// The eight inputs are the settlement's own data set (runtime table, host
// observer, run handle + id, label, card pair, net) — coherent as a unit,
// same shape as `DelegationTool::new` above.
#[allow(clippy::too_many_arguments)]
fn spawn_settlement_delivery(
    runtime: Arc<manox_harness::subagent::SubagentRuntime>,
    observer: Arc<SubagentRunObserver>,
    run: manox_harness::subagent::SubagentRun,
    run_id: String,
    label: String,
    task: Arc<background_task::BackgroundTask>,
    task_id: background_task::TaskId,
    net: Option<Duration>,
) {
    tokio::spawn(async move {
        let _keep_runtime = Arc::clone(&runtime); // the run table outlives the tool call
        let (result, net_fired) = settle_with_net(run.result(), net, || {
            // The net's own cancellation and capacity release. `forget` is
            // the capacity gate's reclamation, not a second settlement: the
            // durable row lands below through the observer. The runtime's
            // settle watcher wakes after the same result and removes a
            // now-missing key (a no-op); a duplicated harness `Event::End` is
            // observation surface, not the durable contract.
            run.dispose();
            runtime.forget(&run_id);
        })
        .await;
        if net_fired {
            // The drive's own settlement — the normal `run/settled` writer —
            // never ran; that loss is why the net fired. Land the durable row
            // here through the same observer, so the two-rows-per-run
            // invariant holds unconditionally. The observer's exactly-once
            // marker turns a recovered drive's later (richer) settlement into
            // a no-op. The salvage carries the net's reason so the durable
            // `stalled` verdict does not claim a silence that never happened.
            observer.on_settled(&RunEndInfo {
                run_id: run_id.clone(),
                provider: String::new(), // descriptive-only; the row never carries it
                label: label.clone(),
                local: true,
                stop_reason: result.stop_reason.clone(),
                output: SETTLEMENT_NET_REASON.to_string(),
            });
        }
        let stats = observer.stats_of(&run_id);
        let delivery = background_delivery(&label, &result, stats);
        observer.retire(&run_id);
        // One settlement vocabulary for every producer: the run's stop
        // reason maps onto it, and the shared mapper writes the
        // wire-stable terminal status.
        let settlement = match result.stop_reason {
            StopReason::Aborted => manox_harness::tasks::Settlement::new(
                manox_harness::tasks::SettlementKind::Stopped,
                manox_harness::tasks::SettlementCause::UserStop,
            ),
            StopReason::Completed => manox_harness::tasks::Settlement::new(
                manox_harness::tasks::SettlementKind::Completed,
                manox_harness::tasks::SettlementCause::Natural,
            ),
            ref reason => manox_harness::tasks::Settlement::new(
                manox_harness::tasks::SettlementKind::Failed,
                manox_harness::tasks::SettlementCause::Natural,
            )
            .with_failure_summary(Some(manox_harness::subagent::verdict(
                &label,
                manox_harness::subagent::kind_of(reason),
            ))),
        };
        background_task::apply_settlement(&task, &task_id, &settlement);
        send_notice(
            &observer.notice_tx,
            BackendNotice::Event(Box::new(ThreadEvent::BackgroundTaskUpdated {
                snapshot: task.snapshot(&task_id),
            })),
            "subagent task terminal snapshot",
        );
        send_notice(
            &observer.notice_tx,
            BackendNotice::SteerDelivered {
                from: manox_harness::steer_bus::AgentId::Subagent(run_id),
                reason: manox_harness::steer_bus::SteerReason::Complete,
                payload: manox_harness::steer_bus::SteerPayload { text: delivery },
            },
            "subagent report delivery",
        );
    });
}

/// The delivery text: verdict line, salvage (or the explicit line that
/// there is none), counters, diagnostic.
fn background_delivery(label: &str, result: &RunResult, stats: Option<(u64, u64)>) -> String {
    let kind = manox_harness::subagent::kind_of(&result.stop_reason);
    let mut text = manox_harness::subagent::verdict(label, kind);
    if result.output.trim().is_empty() {
        text.push(' ');
        text.push_str(manox_harness::subagent::NO_CLOSING_MESSAGE);
    } else {
        text.push_str("\n\n");
        text.push_str(&result.output);
    }
    if let Some((turns, tool_calls)) = stats {
        text.push_str(&format!("\n\n({turns} turns, {tool_calls} tool calls)"));
    }
    if let Some(diagnostic) = &result.diagnostic {
        text.push_str(&format!("\n\n{diagnostic}"));
    }
    text
}

// ── Control tools ────────────────────────────────────────────────────────

/// The `ListAgents` control tool: the live-run table snapshot with the
/// watchdog's health verdict per row (children
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

/// The `InterruptAgent` control tool: cancel one live run
/// (`interrupt_agent`, parent-adjacency enforced structurally — this runtime
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
    /// Run ids whose settlement was already landed, exactly once per
    /// activation: the net's synthetic settlement and a recovered drive's
    /// later (richer) one must not both write a `run/settled` row, and
    /// `retire` cannot clear the marker — it runs immediately after the first
    /// settlement, while the recovered drive arrives later. `on_start` clears
    /// it for a new activation of the same id (a continuation reopens the
    /// run). Entries are bounded by the run count of one assembly.
    settled_once: Mutex<HashSet<String>>,
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
            settled_once: Mutex::new(HashSet::new()),
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

    /// Land one swarm lifecycle row in the owning thread's journal (the
    /// store's typed route; the engine synthesizes the envelope fields).
    /// Two rows per run — `run/started` at dispatch, `run/settled` at the
    /// end — are the whole durable record: progress never rides the
    /// parent journal.
    fn journal_swarm_row(&self, kind: &str, payload: serde_json::Value) {
        // The path is resolved against the disk, not only the store's scan
        // map: the two rows per run are the durable record, and a session
        // whose journal materialized since the last scan (the common case for
        // a fresh session's first turn) must still find its route.
        let path = crate::thread_store::try_global()
            .and_then(|store| store.read(|s| s.journal_path_on_disk(&self.owner_thread_id)));
        if !crate::engine::dispatch_store_journal_row(
            self.owner_thread_id.clone(),
            path,
            kind.to_string(),
            payload,
        ) {
            // A dropped swarm row is a hole in the durable record, not a
            // debug detail: the live delivery still happens, but the log no
            // longer shows the run at all.
            tracing::warn!(
                kind,
                session = %self.owner_thread_id,
                "swarm row has nowhere to land: no live engine route and no journal on disk"
            );
        }
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
}

impl RunObserver for SubagentRunObserver {
    /// A child's `AskParent` delivery: the escalation row is the durable
    /// record; the fold surfaces the question on the parent chat as an
    /// attributed note coaching the continuation.
    fn on_escalate(&self, run_id: &str, question: &str) {
        let auth_id = uuid::Uuid::new_v4().to_string();
        self.journal_swarm_row(
            "run/escalated",
            serde_json::json!({
                "runId": run_id,
                "question": question,
                "authId": auth_id,
            }),
        );
    }

    fn on_start(&self, info: &RunInfo) {
        // A new activation of the run id (a fresh dispatch or a continuation
        // that reopens the same id) starts a new settlement epoch: the
        // previous activation's exactly-once marker must not swallow this
        // one's `run/settled` row.
        self.settled_once
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&info.run_id);
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
        // The run's durable record is the `run/started` row (journaled at
        // dispatch); the live roster is the registry (`ListAgents`) and the
        // chat's `background_work`. Nothing per-tick is forwarded anymore.
        let _ = &subagent_type;
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
        // Health changes surface through the registry (`ListAgents` reads
        // the watchdog directly); no per-tick forwarding exists anymore.
        let _ = health_change;
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
                // The stall line reads from the registry (`ListAgents`
                // serves the watchdog's health directly); the enforcement
                // below stays the tick's only side effect.
                false
            }
            crate::subagent_watchdog::TickOutcome::EnforceStall => true,
        }
    }

    fn on_settled(&self, end: &RunEndInfo) {
        // Exactly-once per activation: the first settlement lands the row;
        // a recovered drive's later settlement is a whole no-op. The net's
        // synthetic row is first by construction, and without this guard the
        // recovered drive would append a second row with zero counters.
        let first = self
            .settled_once
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(end.run_id.clone());
        if !first {
            return;
        }
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
        // The durable settlement row: kind, verdict, salvage, counters —
        // the same vocabulary every consumer reads.
        let kind = manox_harness::subagent::kind_of(&end.stop_reason);
        let verdict = manox_harness::subagent::verdict(&end.label, kind);
        let (turns, tool_calls) = stats.unwrap_or((0, 0));
        self.journal_swarm_row(
            "run/settled",
            serde_json::json!({
                "runId": end.run_id,
                "settlement": kind,
                "verdict": verdict,
                "salvage": (!end.output.trim().is_empty()).then(|| end.output.clone()),
                "turns": turns,
                "toolCalls": tool_calls,
            }),
        );
        // The settled state rides the `run/settled` row and the
        // background-work close; no per-run notice emission remains.
        let _ = &subagent_type;
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
                Some("exceeded its 1000ms budget"),
            ),
        );
        assert!(tool_result.is_error);
        let envelope: serde_json::Value = serde_json::from_str(&text_of(&tool_result)).unwrap();
        assert_eq!(envelope["stop_reason"], "error");
        assert_eq!(envelope["output"], "partial work");
        assert!(
            envelope["error"].as_str().unwrap().contains("budget"),
            "{}",
            envelope["error"]
        );
    }

    /// Every ending delivers: the verdict line, the salvage (or the
    /// explicit line that there is none), the counters, the diagnostic.
    #[test]
    fn background_delivery_mapping() {
        let completed = background_delivery(
            "scout",
            &result(StopReason::Completed, "final report", None),
            None,
        );
        assert!(completed.starts_with("`scout` completed."), "{completed}");
        assert!(completed.ends_with("final report"), "{completed}");

        let failed = background_delivery(
            "scout",
            &result(StopReason::Error, "partial", Some("boom")),
            Some((2, 5)),
        );
        assert!(
            failed.starts_with("`scout` failed before it finished."),
            "{failed}"
        );
        assert!(failed.contains("(2 turns, 5 tool calls)"), "{failed}");
        assert!(failed.ends_with("boom"), "{failed}");

        let timed_out = background_delivery(
            "scout",
            &result(StopReason::TimedOut, "", Some("exceeded its budget")),
            None,
        );
        assert!(
            timed_out.starts_with("`scout` exceeded its time budget"),
            "{timed_out}"
        );
        assert!(
            timed_out.contains(manox_harness::subagent::NO_CLOSING_MESSAGE),
            "{timed_out}"
        );

        // An abort delivers its receipt: the work before the stop is real.
        let aborted = background_delivery(
            "scout",
            &result(StopReason::Aborted, "partial before the stop", None),
            Some((1, 3)),
        );
        assert!(
            aborted.starts_with("`scout` was stopped before it finished."),
            "{aborted}"
        );
        assert!(aborted.contains("partial before the stop"), "{aborted}");
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

    /// The net lets a landing result through untouched: even a result that
    /// takes a while returns verbatim (same stop reason, same output), and
    /// neither the net nor the caller's dispose hook fires.
    #[tokio::test]
    async fn settlement_net_lets_a_landing_result_through_untouched() {
        let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&fired);
        let (settled, net_fired) = settle_with_net(
            async {
                tokio::time::sleep(Duration::from_millis(50)).await;
                result(StopReason::Completed, "the report", None)
            },
            Some(Duration::from_secs(60)),
            move || flag.store(true, std::sync::atomic::Ordering::SeqCst),
        )
        .await;
        assert!(!net_fired);
        assert_eq!(settled.stop_reason, StopReason::Completed);
        assert_eq!(settled.output, "the report");
        assert!(
            !fired.load(std::sync::atomic::Ordering::SeqCst),
            "a result inside the window never fires the net"
        );
    }

    /// A lost result settles synthetically once the net fires: the
    /// watchdog-forced-stall vocabulary with an empty output and a
    /// diagnostic naming the net, after the caller's dispose hook ran.
    #[tokio::test]
    async fn settlement_net_settles_synthetic_stalled_when_the_result_never_lands() {
        let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&fired);
        let (settled, net_fired) = settle_with_net(
            std::future::pending(),
            Some(Duration::from_millis(10)),
            move || flag.store(true, std::sync::atomic::Ordering::SeqCst),
        )
        .await;
        assert!(net_fired);
        assert_eq!(settled.stop_reason, StopReason::Stalled);
        assert!(settled.output.is_empty());
        let diagnostic = settled.diagnostic.expect("the net names itself");
        assert!(diagnostic.contains("settlement net"), "{diagnostic}");
        assert!(
            fired.load(std::sync::atomic::Ordering::SeqCst),
            "the net cancels the run's residual child work first"
        );
    }

    /// A run with no wall-clock budget arms no net: the result is awaited
    /// bare and never resolves, exactly as before the net existed.
    #[tokio::test]
    async fn settlement_net_is_disarmed_without_a_wall_clock_budget() {
        let settled = tokio::time::timeout(
            Duration::from_millis(50),
            settle_with_net(std::future::pending(), None, || {
                panic!("no budget, no net, no fire")
            }),
        )
        .await;
        assert!(
            settled.is_err(),
            "a budgetless run must not settle through a net"
        );
    }

    /// A result that is ready in the same instant the net expires wins: the
    /// sleep arm re-checks the pinned result before synthesizing (select!
    /// picks a ready branch at random, so the loop exercises both orders).
    #[tokio::test]
    async fn settlement_net_yields_to_a_result_ready_at_the_same_instant() {
        for _ in 0..64 {
            let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let flag = Arc::clone(&fired);
            let (settled, net_fired) = settle_with_net(
                std::future::ready(result(StopReason::Completed, "the report", None)),
                Some(Duration::ZERO),
                move || flag.store(true, std::sync::atomic::Ordering::SeqCst),
            )
            .await;
            assert!(!net_fired, "a ready result wins the tie");
            assert_eq!(settled.stop_reason, StopReason::Completed);
            assert_eq!(settled.output, "the report");
            assert!(
                !fired.load(std::sync::atomic::Ordering::SeqCst),
                "the net never fires when the result is already ready"
            );
        }
    }

    fn run_info(run_id: &str) -> RunInfo {
        RunInfo {
            run_id: run_id.to_string(),
            provider: "spawn".to_string(),
            label: "scout".to_string(),
            kind: "Explore".to_string(),
            model: None,
            local: true,
            budgets: Budgets::default(),
            tool_call_id: None,
        }
    }

    fn settled_info(run_id: &str, stop_reason: StopReason, output: &str) -> RunEndInfo {
        RunEndInfo {
            run_id: run_id.to_string(),
            provider: "spawn".to_string(),
            label: "scout".to_string(),
            local: true,
            stop_reason,
            output: output.to_string(),
        }
    }

    /// Feed tool-call events into a live row's watchdog (the run counters).
    fn feed_tool_calls(observer: &SubagentRunObserver, run_id: &str, count: usize) {
        let runs = observer.locked();
        let watch = runs.get(run_id).expect("the run has a live row");
        let mut wd = watch.watchdog.lock().unwrap_or_else(|e| e.into_inner());
        for i in 0..count {
            let _ = wd.observe(
                &manox_harness::types::AgentEvent::ToolExecutionStart {
                    tool_call_id: format!("call-{i}"),
                    tool_name: "Read".to_string(),
                    arguments: serde_json::json!({}),
                },
                std::time::Instant::now(),
            );
        }
    }

    /// Settlement is exactly-once per activation. The first `on_settled`
    /// lands the row and moves the counters; a recovered drive's later (and
    /// richer) settlement is a whole no-op, so the settled counters keep the
    /// first settlement's values instead of being overwritten (and no second
    /// zero-counter row is journaled). A continuation reopens the same run
    /// id, and `on_start` opens a new epoch so its settlement lands again.
    #[test]
    fn settlement_is_exactly_once_per_activation() {
        // The durable-row route needs the process-global runtime handle.
        crate::runtime::init_hermetic_for_test();
        let (notice_tx, _notice_rx) = mpsc::unbounded_channel();
        let observer = SubagentRunObserver::new("thread-1".to_string(), notice_tx);

        observer.on_start(&run_info("sub-1"));
        feed_tool_calls(&observer, "sub-1", 2);
        observer.on_settled(&settled_info(
            "sub-1",
            StopReason::Stalled,
            SETTLEMENT_NET_REASON,
        ));
        assert_eq!(observer.stats_of("sub-1"), Some((0, 2)));
        assert!(
            observer
                .settled_once
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains("sub-1"),
            "the first settlement marks the run"
        );

        // A recovered drive re-publishes a live row and lands a richer late
        // settlement. The marker makes the whole call a no-op.
        observer.locked().insert(
            "sub-1".to_string(),
            RunWatch {
                subagent_type: "Explore".to_string(),
                watchdog: Arc::new(Mutex::new(SubagentWatchdog::new(
                    std::time::Instant::now(),
                    None,
                ))),
            },
        );
        feed_tool_calls(&observer, "sub-1", 5);
        observer.on_settled(&settled_info("sub-1", StopReason::Completed, "late"));
        assert_eq!(
            observer
                .settled
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get("sub-1")
                .map(|s| (s.turns, s.tool_calls)),
            Some((0, 2)),
            "a late settlement never overwrites the first one"
        );

        // A continuation reopens the same run id: `on_start` clears the
        // marker, so the continued activation settles with its own counters.
        observer.on_start(&run_info("sub-1"));
        feed_tool_calls(&observer, "sub-1", 1);
        observer.on_settled(&settled_info("sub-1", StopReason::Completed, "continued"));
        assert_eq!(
            observer
                .settled
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get("sub-1")
                .map(|s| (s.turns, s.tool_calls)),
            Some((0, 1)),
            "a new activation settles again"
        );
    }

    /// A provider whose run never produces a result: the drive task is alive
    /// but the settlement never lands, the exact case the net covers.
    struct PendingProvider;

    #[async_trait::async_trait]
    impl manox_harness::subagent::SubagentProvider for PendingProvider {
        fn name(&self) -> &str {
            "pending"
        }

        fn capabilities(&self) -> manox_harness::subagent::Capabilities {
            manox_harness::subagent::Capabilities {
                agent_options: false,
                output_schema: false,
                depth_limit: false,
                tool_filter: false,
                persona: false,
            }
        }

        async fn start(
            &self,
            request: manox_harness::subagent::types::ResolvedStartRequest,
        ) -> Result<manox_harness::subagent::SubagentRun, manox_harness::subagent::SubagentError>
        {
            let cancel = request.inner.cancel.clone();
            Ok(manox_harness::subagent::SubagentRun::new(
                request.run_id,
                cancel,
                Box::pin(std::future::pending()),
            ))
        }
    }

    /// When the net fires it releases the runtime's capacity slot: without
    /// `forget` the wedged entry and its permit would survive forever, and
    /// four such runs would reject every later delegation at the cap. The
    /// delivery then retires the observer's own state.
    #[tokio::test]
    async fn settlement_net_releases_the_runtime_slot_it_settles() {
        // The delivery journals its durable row through the global runtime.
        crate::runtime::init_hermetic_for_test();
        let runtime = manox_harness::subagent::SubagentRuntime::new();
        runtime.register_provider_permanent(Arc::new(PendingProvider));
        let (notice_tx, mut notice_rx) = mpsc::unbounded_channel();
        let observer = SubagentRunObserver::new("thread-1".to_string(), notice_tx);
        let run = runtime
            .start(
                "pending",
                manox_harness::subagent::test_request("scout", "do it"),
            )
            .await
            .expect("start");
        let run_id = run.id.clone();
        observer.on_start(&run_info(&run_id));
        assert!(runtime.is_live(&run_id));

        let cancel = run.dispose_token().clone();
        let (task_id, task) =
            background_task::register("thread-1".to_string(), "scout".to_string(), cancel);
        spawn_settlement_delivery(
            Arc::clone(&runtime),
            Arc::clone(&observer),
            run,
            run_id.clone(),
            "scout".to_string(),
            task,
            task_id,
            Some(Duration::from_millis(10)),
        );

        // The terminal notice is sent after the net settled, forgot the run,
        // and retired the observer state, so receiving it proves the funnel.
        let notice = tokio::time::timeout(Duration::from_secs(5), notice_rx.recv())
            .await
            .expect("the net settles the run");
        assert!(notice.is_some(), "the settlement delivers a notice");
        assert!(
            !runtime.is_live(&run_id),
            "the net releases the capacity slot"
        );
        assert!(runtime.list_runs().is_empty());
        assert!(
            observer.stats_of(&run_id).is_none(),
            "the delivery retires the observer state"
        );
    }
}
