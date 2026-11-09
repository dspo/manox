//! Host-side `AgentBus` + `Steer` tool — the Captain's unified subagent
//! scheduling surface. One tool covers the whole loop: `Dispatch` with
//! `to.spawn` = a definition name creates a run, `Dispatch` with
//! `to.agent_address` = a run id (no spawn) continues it, `Inject` guides a
//! live run mid-flight through the kernel's intra-session steer
//! (`HarnessHandle::steer`), and `Abort` stops it. Definition
//! resolution stays engine-internal config ([`crate::subagent`]) that the
//! run arms route through; `continue_run_id` has no compat alias —
//! continuation is `Dispatch` + run id, period. The bus also retains the
//! TeamMember member operations (real manox Threads), which are pure gpui
//! facade round trips (`BusOp`). The cross-session agent bus is a manox
//! host extension.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, Weak};

use manox_harness::steer_bus::{AgentId, BusOp, SteerPayload, SteerReason, ToSpec};
use manox_harness::tool::{AgentTool, AgentToolResult, ToolError};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::subagent::{SubagentCatalog, SubagentDispatcher, SubagentRunObserver};
use crate::thread_engine::{BackendNotice, send_notice};

/// The literal `to.spawn` value that spawns a real manox Thread (the frozen
/// member arm). Every other spawn value names a subagent definition.
pub(crate) const MEMBER_SPAWN: &str = "TeamMember";

/// The engine-installed subagent scheduling surface: the runtime run table,
/// the host observer, the dispatch path, and the definition catalog. The
/// bus is engine-scoped (it survives session swaps so a user cancel always
/// reaches every derivative this thread spawned); each session assembly
/// replaces the surface, because the runtime and its run-id namespace are
/// assembly-scoped.
pub(crate) struct SubagentDelegation {
    pub(crate) runtime: Arc<manox_harness::subagent::SubagentRuntime>,
    pub(crate) observer: Arc<SubagentRunObserver>,
    pub(crate) dispatcher: Arc<SubagentDispatcher>,
    pub(crate) catalog: Arc<SubagentCatalog>,
}

/// What the calling tool hands the bus beyond the kernel payload: the tool
/// call id (the run row's join key), the turn's cancellation token (a
/// foreground dispatch's cancel channel), and the Dispatch-only optional
/// parameters as validated JSON.
pub(crate) struct SteerCall {
    pub(crate) tool_call_id: String,
    pub(crate) signal: CancellationToken,
    pub(crate) dispatch_params: serde_json::Value,
}

// ── AgentBus ─────────────────────────────────────────────────────────────

/// The host-side agent bus. One per thread (Captain or member). Tracks the
/// member thread ids this thread spawned so Inject/Abort/cancel-fan-out can
/// address them, and carries the subagent scheduling surface the Captain's
/// run arms route through.
pub struct AgentBus {
    pub(crate) owner_thread_id: String,
    pub(crate) notice_tx: mpsc::UnboundedSender<BackendNotice>,
    spawned_members: Mutex<HashSet<String>>,
    delegation: Mutex<Option<Arc<SubagentDelegation>>>,
    weak_self: Mutex<Weak<AgentBus>>,
}

/// Poison-tolerant lock: a holder that panicked must not take every later
/// dispatch down with a secondary panic.
fn locked<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

impl AgentBus {
    /// Construct the bus and return it wrapped in an `Arc`. The `weak_self`
    /// field is set internally so `steer()` can create child `SteerTool`
    /// instances that share this bus.
    pub fn new(
        owner_thread_id: String,
        notice_tx: mpsc::UnboundedSender<BackendNotice>,
    ) -> Arc<Self> {
        let bus = Arc::new(Self {
            owner_thread_id,
            notice_tx,
            spawned_members: Mutex::new(HashSet::new()),
            delegation: Mutex::new(None),
            weak_self: Mutex::new(Weak::new()),
        });
        *bus.weak_self.lock().unwrap() = Arc::downgrade(&bus);
        bus
    }

    pub fn owner_thread_id(&self) -> &str {
        &self.owner_thread_id
    }

    /// Install (or replace, on a session re-assembly) the subagent
    /// scheduling surface. Called from the tool assembly after the runtime
    /// and observer exist.
    pub(crate) fn install_delegation(&self, delegation: SubagentDelegation) {
        *locked(&self.delegation) = Some(Arc::new(delegation));
    }

    /// Test-support: install a scheduling surface from caller-supplied
    /// parts (a runtime with providers already registered, the host
    /// observer, the definition catalog). Integration tests outside this
    /// crate drive the real Steer → runtime chain without a full engine
    /// assembly; the tool assembly is the production installer.
    #[cfg(any(test, feature = "test-support"))]
    pub fn install_delegation_for_test(
        &self,
        runtime: Arc<manox_harness::subagent::SubagentRuntime>,
        observer: Arc<crate::subagent::SubagentRunObserver>,
        defs: Vec<manox_harness::subagent::SubagentDefConfig>,
    ) {
        self.install_delegation(SubagentDelegation {
            runtime,
            observer: Arc::clone(&observer),
            dispatcher: Arc::new(crate::subagent::SubagentDispatcher {
                observer,
                provider_registry: None,
                env: Arc::new(manox_harness::env::TokioExecutionEnv::new(
                    std::env::temp_dir(),
                )),
                cwd: std::env::temp_dir(),
                parent_depth: 0,
                parent_session: Some(self.owner_thread_id().to_string()),
            }),
            catalog: Arc::new(crate::subagent::SubagentCatalog::new(defs)),
        });
    }

    fn delegation(&self) -> Option<Arc<SubagentDelegation>> {
        locked(&self.delegation).clone()
    }

    fn require_delegation(&self) -> Result<Arc<SubagentDelegation>, ToolError> {
        self.delegation().ok_or_else(|| {
            ToolError::ExecutionFailed("no subagent surface is installed on this thread".into())
        })
    }

    /// Whether `addr` names a live subagent run of the installed surface.
    pub(crate) fn is_live_run(&self, addr: &str) -> bool {
        self.delegation().is_some_and(|d| d.runtime.is_live(addr))
    }

    /// Whether `addr` names a run that already settled (in this assembly).
    pub(crate) fn is_settled_run(&self, addr: &str) -> bool {
        self.delegation()
            .is_some_and(|d| d.observer.has_settled(addr))
    }

    /// Interrupt every live run on the installed surface (the Steer Abort
    /// primitive applied to the whole live table — the stop-all face).
    /// Cancellation rides each run's dispose token, so this returns once
    /// issued; each settlement lands through the existing delivery paths
    /// with the usual `aborted` verdict. Returns the run ids the
    /// cancellation was issued to (a snapshot — the run may settle after).
    pub fn interrupt_all_runs(&self) -> Vec<String> {
        let Some(delegation) = self.delegation() else {
            return Vec::new();
        };
        let ids: Vec<String> = delegation
            .runtime
            .list_runs()
            .into_iter()
            .map(|run| run.id)
            .collect();
        for id in &ids {
            delegation.runtime.interrupt(id);
        }
        ids
    }

    /// The main Steer routing entry point. Run addresses resolve against the
    /// installed subagent surface; member addresses against the spawned
    /// member set; a Captain address that resolves to neither run nor member
    /// gets the address-contract coach (the permission wall answers only the
    /// non-Captain callers).
    pub(crate) async fn steer(
        &self,
        from: AgentId,
        to: ToSpec,
        reason: SteerReason,
        payload: SteerPayload,
        call: SteerCall,
    ) -> Result<AgentToolResult, ToolError> {
        let addr = to.agent_address.as_str();
        match (&from, &reason) {
            // ── Dispatch: spawn TeamMember (real thread) ───────────────
            (AgentId::Captain, SteerReason::Dispatch)
                if to.spawn.as_deref() == Some(MEMBER_SPAWN) =>
            {
                // The frozen member arm carries the prompt alone — a stray
                // dispatch budget would silently run under a different
                // contract than asked.
                if let Some(stray) = first_stray_dispatch_param(&call.dispatch_params) {
                    return Err(ToolError::InvalidArguments(format!(
                        "`{stray}` only applies to a subagent run dispatch — a TeamMember \
                         spawn carries the `prompt` alone."
                    )));
                }
                self.dispatch_member(addr, &payload.text).await
            }

            // ── Dispatch: create a subagent run from a definition ──────
            (AgentId::Captain, SteerReason::Dispatch) if to.spawn.is_some() && addr.is_empty() => {
                let spawn = to.spawn.clone().unwrap_or_default();
                self.dispatch_run(&spawn, &call).await
            }

            // ── Dispatch: continue a run through an explicit definition ─
            (AgentId::Captain, SteerReason::Dispatch) if to.spawn.is_some() => {
                let spawn = to.spawn.clone().unwrap_or_default();
                let delegation = self.require_delegation()?;
                Self::reject_live_continuation(&delegation, addr)?;
                // The explicit definition must be the run's own: a mismatch
                // would reopen the child under a different persona than the
                // one its context was built by. The exception is the
                // recovery path — the remembered kind is no longer
                // registered, so a current definition takes over.
                match delegation.observer.kind_of(addr) {
                    Some(remembered) => match delegation.catalog.get(&remembered) {
                        Some(config) if remembered == spawn => {
                            self.continue_dispatch(&delegation, config, addr, &call)
                                .await
                        }
                        Some(_) => Err(ToolError::InvalidArguments(format!(
                            "run `{addr}` was dispatched through definition `{remembered}`, \
                             not `{spawn}`; continue it with `to.agent_address` alone, or \
                             with `to.spawn: \"{remembered}\"`"
                        ))),
                        // The remembered kind is no longer registered: the
                        // explicit definition is the takeover path.
                        None => {
                            let config = delegation
                                .catalog
                                .get(&spawn)
                                .ok_or_else(no_such_definition(&spawn))?;
                            self.continue_dispatch(&delegation, config, addr, &call)
                                .await
                        }
                    },
                    None => Err(unknown_run(addr)),
                }
            }

            // ── Dispatch: continue a run through its own definition ────
            (AgentId::Captain, SteerReason::Dispatch) => {
                if addr.is_empty() {
                    return Err(ToolError::InvalidArguments(
                        "a run dispatch needs `to.spawn` = a definition name (see \
                         ListAgents); continuing a run needs `to.agent_address` = its run id"
                            .into(),
                    ));
                }
                let delegation = self.require_delegation()?;
                Self::reject_live_continuation(&delegation, addr)?;
                let kind = delegation
                    .observer
                    .kind_of(addr)
                    .ok_or_else(|| unknown_run(addr))?;
                let config = delegation.catalog.get(&kind).ok_or_else(|| {
                    ToolError::InvalidArguments(format!(
                        "run `{addr}` dispatched through definition `{kind}`, which is no \
                         longer registered; continue it with `to.spawn` naming a current \
                         definition (see ListAgents)"
                    ))
                })?;
                self.continue_dispatch(&delegation, config, addr, &call)
                    .await
            }

            // ── Inject to member thread ─────────────────────────────────
            (AgentId::Captain, SteerReason::Inject) if self.is_member(addr) => self
                .bus_request(BusOp::InjectMember {
                    thread_id: addr.to_string(),
                    payload: payload.text.clone(),
                })
                .await
                .map(|_| ack(addr, false, None)),

            // ── Inject into a live run (mid-run guidance) ───────────────
            (AgentId::Captain, SteerReason::Inject) if self.is_live_run(addr) => {
                self.guide_run(addr, &payload.text).await
            }

            // ── Inject into a settled run: coach the continuation ───────
            (AgentId::Captain, SteerReason::Inject) if self.is_settled_run(addr) => {
                Err(ToolError::ExecutionFailed(format!(
                    "run `{addr}` has settled — Inject only reaches a live run. To rework it, \
                     dispatch to the run id (`reason: \"Dispatch\"`, `to.agent_address: \
                     \"{addr}\"`, no `to.spawn`) and the child continues with its full context."
                )))
            }

            // ── Inject to an unknown address: name the address contract ─
            (AgentId::Captain, SteerReason::Inject) => Err(unknown_run(addr)),

            // ── Abort member thread ─────────────────────────────────────
            (AgentId::Captain, SteerReason::Abort) if self.is_member(addr) => self
                .bus_request(BusOp::AbortMember {
                    thread_id: addr.to_string(),
                })
                .await
                .map(|_| ack(addr, false, None)),

            // ── Abort a live run ────────────────────────────────────────
            (AgentId::Captain, SteerReason::Abort) if self.is_live_run(addr) => {
                let delegation = self.require_delegation()?;
                if !delegation.runtime.interrupt(addr) {
                    // Lost the race with settlement: no live run took the
                    // cancel, so `interrupted: true` would be a lie.
                    return Err(ToolError::ExecutionFailed(format!(
                        "run `{addr}` is no longer live (it just settled); to rework it, \
                         dispatch to the run id (`reason: \"Dispatch\"`, `to.agent_address: \
                         \"{addr}\"`)"
                    )));
                }
                Ok(AgentToolResult::text(
                    serde_json::json!({
                        "aborted": true,
                        "agent_address": addr,
                        "interrupted": true,
                    })
                    .to_string(),
                ))
            }

            // ── Abort a settled run: nothing live to cancel ─────────────
            (AgentId::Captain, SteerReason::Abort) if self.is_settled_run(addr) => {
                Err(ToolError::InvalidArguments(format!(
                    "run `{addr}` has already settled; only a live run can be aborted"
                )))
            }

            // ── Abort an unknown address: name the address contract ─────
            (AgentId::Captain, SteerReason::Abort) => Err(unknown_run(addr)),

            // ── Permission denied ───────────────────────────────────────
            _ => Err(ToolError::InvalidArguments(format!(
                "steer not allowed: from={from:?} to={addr} reason={reason:?} — the \
                 Captain's Steer dispatches, guides, and stops subagent runs and messages \
                 TeamMember threads; a subagent cannot steer"
            ))),
        }
    }

    /// A continuation addresses a SETTLED run: a run id that is still live
    /// would otherwise re-open the child mid-flight (and at the runtime
    /// layer overwrite its live-table entry, dropping the capacity permit
    /// and dispose token). The refusal names the mid-run alternatives.
    fn reject_live_continuation(
        delegation: &Arc<SubagentDelegation>,
        addr: &str,
    ) -> Result<(), ToolError> {
        if delegation.runtime.is_live(addr) {
            return Err(ToolError::InvalidArguments(format!(
                "run `{addr}` is still live — Inject to guide it, Abort to stop it; \
                 continuations address settled runs"
            )));
        }
        Ok(())
    }

    /// Check if `addr` is a known member thread id.
    fn is_member(&self, addr: &str) -> bool {
        locked(&self.spawned_members).contains(addr)
    }

    /// Dispatch a TeamMember (real thread): send BusRequest to facade.
    async fn dispatch_member(
        &self,
        name: &str,
        prompt: &str,
    ) -> Result<AgentToolResult, ToolError> {
        let result = self
            .bus_request(BusOp::SpawnMember {
                name: name.to_string(),
                prompt: prompt.to_string(),
            })
            .await?;
        // result is the thread id.
        locked(&self.spawned_members).insert(result.clone());
        Ok(ack(name, true, Some(result)))
    }

    /// Create a subagent run: resolve the definition from the catalog and
    /// ride the one dispatch path (budgets, ceilings, isolation, foreground
    /// wait or background card).
    async fn dispatch_run(
        &self,
        spawn: &str,
        call: &SteerCall,
    ) -> Result<AgentToolResult, ToolError> {
        let delegation = self.require_delegation()?;
        let config = delegation
            .catalog
            .get(spawn)
            .ok_or_else(no_such_definition(spawn))?;
        let resolved = delegation
            .dispatcher
            .resolve_dispatch(config, &call.dispatch_params)?;
        delegation
            .dispatcher
            .dispatch(
                &delegation.runtime,
                config,
                resolved,
                &call.tool_call_id,
                &call.signal,
            )
            .await
    }

    /// Continue a run: same run id, reopened child session. The definition
    /// config rides along (persona / filter / model ceilings), because the
    /// reopened activation re-derives its session from the request.
    async fn continue_dispatch(
        &self,
        delegation: &Arc<SubagentDelegation>,
        config: &manox_harness::subagent::SubagentDefConfig,
        run_id: &str,
        call: &SteerCall,
    ) -> Result<AgentToolResult, ToolError> {
        let mut resolved = delegation
            .dispatcher
            .resolve_dispatch(config, &call.dispatch_params)?;
        resolved.continuation = Some(run_id.to_string());
        delegation
            .dispatcher
            .dispatch(
                &delegation.runtime,
                config,
                resolved,
                &call.tool_call_id,
                &call.signal,
            )
            .await
    }

    /// Guide a live run mid-flight: the kernel's steer primitive injects the
    /// text into the child's current (or next) turn; the durable witness is
    /// one `run/guided` row in the parent journal, and it lands BEFORE the
    /// delivery — a crash between the kernel steer and the row append must
    /// not lose the guidance record (the row is written first; a run that
    /// settles in the race window bounces the delivery, and the coaching
    /// error keeps the ack delivery-only). The row is the durable witness of
    /// the delivery ATTEMPT, not a delivery guarantee: a bounced delivery
    /// still leaves the row, and what was actually delivered is what the
    /// ack reports. The run's idle watchdog treats the guidance as activity.
    /// The ack confirms DELIVERY — it is never the child's answer.
    async fn guide_run(&self, run_id: &str, text: &str) -> Result<AgentToolResult, ToolError> {
        let delegation = self.require_delegation()?;
        let message = manox_harness::types::AgentMessage::user(text.to_string());
        // Durable witness of the ATTEMPT first (the disposition above), then
        // the delivery — what was actually delivered is what the ack reports.
        delegation.observer.journal_swarm_row(
            "run/guided",
            serde_json::json!({
                "runId": run_id,
                "text": text,
            }),
        );
        if !manox_harness::subagent::feeds::steer_child(run_id, message) {
            // Lost the race with settlement: coach the continuation contract.
            return Err(ToolError::ExecutionFailed(format!(
                "run `{run_id}` is no longer live (it just settled); to rework it, dispatch \
                 to the run id (`reason: \"Dispatch\"`, `to.agent_address: \"{run_id}\"`)"
            )));
        }
        delegation.observer.note_guidance(run_id);
        Ok(guided_ack(run_id))
    }

    /// Send a BusRequest to the facade (gpui main thread) and await reply.
    async fn bus_request(&self, op: BusOp) -> Result<String, ToolError> {
        let (tx, rx) = async_channel::bounded(1);
        self.notice_tx
            .send(BackendNotice::BusRequest {
                op,
                responder: Some(tx),
            })
            .map_err(|_| ToolError::ExecutionFailed("engine actor gone".into()))?;
        rx.recv()
            .await
            .map_err(|_| ToolError::ExecutionFailed("bus request dropped".into()))?
            .map_err(ToolError::ExecutionFailed)
    }

    /// Fire-and-forget user-cancel fan-out: one `BusOp::AbortMember` per
    /// spawned member thread. The replies are discarded — a cancel must
    /// never block the gpui thread on member round trips, and each member's
    /// facade-side handler recurses into that member's own derivatives.
    pub fn abort_all_members(&self) {
        let members: Vec<String> = locked(&self.spawned_members).iter().cloned().collect();
        for thread_id in members {
            send_notice(
                &self.notice_tx,
                BackendNotice::BusRequest {
                    op: BusOp::AbortMember { thread_id },
                    responder: None,
                },
                "steer bus request",
            );
        }
    }

    /// Test-only: register a member id as spawned (production inserts via
    /// the `dispatch_member` facade round trip).
    #[cfg(any(test, feature = "test-support"))]
    pub fn register_spawned_member_for_test(&self, thread_id: &str) {
        locked(&self.spawned_members).insert(thread_id.to_string());
    }
}

fn no_such_definition(spawn: &str) -> impl FnOnce() -> ToolError + '_ {
    move || {
        ToolError::InvalidArguments(format!(
            "no subagent definition `{spawn}`; call ListAgents and take `to.spawn` values \
             from its definitions list"
        ))
    }
}

/// The unknown-address coach, shared by the continuation arm and the
/// Inject/Abort arms' explicit unknown-address refusals: run ids come from
/// dispatch results, never from thin air.
fn unknown_run(addr: &str) -> ToolError {
    ToolError::InvalidArguments(format!(
        "no subagent run `{addr}` is known to this thread; take run ids from the Dispatch \
         results (or ListAgents) and address them from there"
    ))
}

/// The first Dispatch-only parameter carrying a non-null value, if any —
/// the fail-loud check the non-run arms apply.
fn first_stray_dispatch_param(params: &serde_json::Value) -> Option<&'static str> {
    DISPATCH_PARAMS
        .iter()
        .copied()
        .find(|p| params.get(*p).is_some_and(|v| !v.is_null()))
}

/// Build a JSON ack tool_result.
fn ack(addr: &str, spawned: bool, thread_id: Option<String>) -> AgentToolResult {
    AgentToolResult::text(
        serde_json::json!({
            "delivered": true,
            "agent_address": addr,
            "spawned": spawned,
            "thread_id": thread_id,
        })
        .to_string(),
    )
}

/// The guidance ack: delivery confirmation only — never the child's answer.
fn guided_ack(run_id: &str) -> AgentToolResult {
    AgentToolResult::text(
        serde_json::json!({
            "delivered": true,
            "agent_address": run_id,
        })
        .to_string(),
    )
}

// ── SteerTool ────────────────────────────────────────────────────────────

/// The model-facing Steer tool — the Captain's single subagent scheduling
/// surface (create / continue / guide / abort) plus the frozen TeamMember
/// spawn arm. Subagents never receive it (their snapshots strip it).
pub struct SteerTool {
    bus: Arc<AgentBus>,
    from: AgentId,
}

impl SteerTool {
    pub fn new(bus: Arc<AgentBus>, from: AgentId) -> Self {
        Self { bus, from }
    }
}

/// The Dispatch-only optional parameters from the delegation schema.
/// Rejected on a non-Dispatch reason.
/// `description` is not among them: it is only meaningful on run dispatches
/// and enforced there fail-loud (the schema does not globally require it —
/// Inject/Abort need no label — so it may ride along harmlessly, never a
/// different contract).
const DISPATCH_PARAMS: &[&str] = &[
    "run_in_background",
    "timeout_ms",
    "idle_timeout_ms",
    "isolation",
    "permissions",
    "model",
    "reasoning_effort",
    "replay_thinking",
];

/// The `Steer` description: the model-facing contract of the whole loop —
/// create, continue, guide, abort — with the address lifecycle (the ack's
/// run id is the permanent address) and the definition discovery entry
/// point. Model-facing English — never localized.
const STEER_DESCRIPTION: &str = "Steer your subagents and TeamMembers — the single surface for \
     their whole lifecycle. Subagents are fresh-context workers running a named definition \
     (pick `to.spawn` values from ListAgents' definitions list; never guess): \
     `reason: \"Dispatch\"` with `to.spawn` = a definition name and `prompt` = the task \
     creates one; the result carries the run's id, the run's permanent address. By default \
     the call waits and returns the child's final report; `run_in_background: true` parks \
     it on a background card and the report arrives as a message on settlement. \
     `timeout_ms`/`idle_timeout_ms` arm kill budgets that deliver a partial report on \
     expiry; `isolation: \"worktree\"` runs the child in a throwaway git worktree; the \
     optional `permissions` object ({ mode: inherit|sandbox|full-access, network: \
     inherit|off|on }) narrows the child's access for this dispatch only — never wider \
     than the definition's ceiling or this session's authority; `model`/`reasoning_effort` \
     override the route; `replay_thinking` replays the child's earlier thinking blocks. \
     Guide a live run mid-flight with `reason: \"Inject\"`, `to.agent_address` = the run \
     id — the guidance lands in the child's current (or next) turn and counts as activity \
     for its idle budget, and the ack confirms delivery (it is NOT the child's answer). \
     Stop a run with `reason: \"Abort\"`. Continue a settled run by dispatching to it: \
     `reason: \"Dispatch\"`, `to.agent_address` = the run id, no `to.spawn`, and the \
     follow-up as `prompt` — the child keeps its full context. Continuation addresses a \
     SETTLED run: a run that is still live takes Inject (to guide) or Abort (to stop), \
     never a re-dispatch. A takeover continuation — the run's remembered \
     definition deregistered, `to.spawn` naming a current one — is re-governed \
     under the NEW definition's tool filter and permission ceiling. \
     `description` is the required short task label for run dispatches only — \
     Inject and Abort need none. To create a real manox Thread that persists, \
     appears in the sidebar, and \
     can be resumed, spawn `to.spawn: \"TeamMember\"` (Captain only; its reply carries \
     the thread id).";

#[async_trait::async_trait]
impl AgentTool for SteerTool {
    fn name(&self) -> &str {
        crate::tools::STEER
    }

    fn description(&self) -> &str {
        STEER_DESCRIPTION
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
                "to": {
                    "type": "object",
                    "properties": {
                        "agent_address": {
                            "type": "string",
                            "description": "Target address. Inject/Abort: the live run's id (from the dispatch result). Continue: a SETTLED run's id — a live run takes Inject or Abort. TeamMember ops: the member's name or thread id. Omit when creating a new run. Reserved: 'Captain', 'User'."
                        },
                        "spawn": {
                            "type": "string",
                            "description": "The subagent definition to create — take values from ListAgents' definitions list — or the literal \"TeamMember\" for a real manox Thread (Captain only, process: persisted, sidebar-visible, resumable, own Captain session + own bus; error if the name exists). Omit when addressing an existing run by id."
                        }
                    },
                    "description": "Target of the steer.",
                    "additionalProperties": false
                },
                "reason": {
                    "type": "string",
                    "enum": ["Dispatch", "Inject", "Abort"],
                    "description": "Dispatch creates a run (to.spawn = definition) or continues a SETTLED one (to.agent_address = run id, no to.spawn — a live run takes Inject or Abort); Inject messages a live run mid-flight; Abort cancels it."
                },
                "prompt": {
                    "type": "string",
                    "description": "Payload: the full task or follow-up (Dispatch — the child only knows what you write), the guidance message (Inject). Abort ignores it."
                },
                "description": {
                    "type": "string",
                    "description": "Required for run dispatches (the short task label shown on the run card, used as the child's creation label); Inject/Abort need none."
                },
                "model": {
                    "type": "string",
                    "description": "Optional model override for a run dispatch (provider::model, or provider::model::effort)."
                },
                "reasoning_effort": {
                    "type": "string",
                    "enum": ["off", "low", "medium", "high", "max"],
                    "description": "Optional reasoning effort for the child's model."
                },
                "replay_thinking": {
                    "type": "boolean",
                    "description": "Replay the child's thinking blocks from earlier turns on later requests. Off by default: historical reasoning is dropped to save tokens, while the current turn's thinking is always kept."
                },
                "run_in_background": {
                    "type": "boolean",
                    "description": "Run on a background card and deliver the report as a message on settlement instead of blocking this call until the final report. A definition declaring background:true always dispatches to the background; this flag cannot foreground it."
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "Optional wall-clock budget in milliseconds (min 1000, max 30 days). On expiry the child is terminated and a report with its partial output is delivered."
                },
                "idle_timeout_ms": {
                    "type": "integer",
                    "description": "Optional idle budget in milliseconds (min 1000, max 30 days): the longest stretch with no activity and no tool running before the child is terminated as stalled. Enforcement granularity is ~5s. Injected guidance counts as activity and resets this budget."
                },
                "isolation": {
                    "type": "string",
                    "enum": ["worktree"],
                    "description": "Run the child in a throwaway git worktree; a worktree with committed work is kept and reported."
                },
                "permissions": {
                    "type": "object",
                    "description": "Optional per-dispatch permission override for this child. Narrowing only: each named axis must stay within the definition's declared ceiling on that axis — a wider naming is refused — unnamed axes keep the declared ceiling, and nothing can exceed this session's own authority.",
                    "properties": {
                        "mode": {
                            "type": "string",
                            "enum": ["inherit", "sandbox", "full-access"],
                            "description": "`sandbox` confines the child's Bash regardless of this session's mode; `inherit`/`full-access` both follow this session's live mode."
                        },
                        "network": {
                            "type": "string",
                            "enum": ["inherit", "off", "on"],
                            "description": "`off` keeps the child's confined commands off the network regardless of this session's switch; `inherit`/`on` both follow this session's live route."
                        }
                    },
                    "additionalProperties": false
                }
            },
            "required": ["to", "reason", "prompt"],
            "additionalProperties": false
        })
    }

    async fn execute(
        &self,
        tool_call_id: &str,
        params: serde_json::Value,
        signal: CancellationToken,
        _ctx: &dyn manox_harness::tool::ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        // Fail-loud shape validation: an unknown field is a typo'd contract,
        // not a silent no-op.
        let unknown: Vec<&String> = params
            .as_object()
            .ok_or_else(|| ToolError::InvalidArguments("params must be an object".into()))?
            .keys()
            .filter(|k| {
                k.as_str() != "to"
                    && k.as_str() != "reason"
                    && k.as_str() != "prompt"
                    && k.as_str() != "description"
            })
            .filter(|k| !DISPATCH_PARAMS.contains(&k.as_str()))
            .collect();
        if !unknown.is_empty() {
            return Err(ToolError::InvalidArguments(format!(
                "unknown field(s) {unknown:?}; Steer takes `to`, `reason`, `prompt`, \
                 `description`, and the Dispatch-only {}",
                DISPATCH_PARAMS
                    .iter()
                    .map(|p| format!("`{p}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        let to = params
            .get("to")
            .ok_or_else(|| ToolError::InvalidArguments("'to' is required".into()))?;
        let to_unknown: Vec<&String> = to
            .as_object()
            .ok_or_else(|| ToolError::InvalidArguments("'to' must be an object".into()))?
            .keys()
            .filter(|k| k.as_str() != "agent_address" && k.as_str() != "spawn")
            .collect();
        if !to_unknown.is_empty() {
            return Err(ToolError::InvalidArguments(format!(
                "`to` has unknown field(s) {to_unknown:?}; only `agent_address` and `spawn` exist."
            )));
        }
        let agent_address = to.get("agent_address").and_then(|v| v.as_str());
        let spawn = to.get("spawn").and_then(|v| v.as_str());
        let reason_str = params
            .get("reason")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArguments("'reason' is required".into()))?;
        let prompt = params
            .get("prompt")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArguments("'prompt' is required".into()))?;

        if reason_str == "Complete" {
            return Err(ToolError::ExecutionFailed(
                "Complete is harness-emitted on termination, not callable as a tool".into(),
            ));
        }
        if spawn == Some("Captain") || spawn == Some("User") {
            return Err(ToolError::InvalidArguments(
                "cannot spawn User or Captain".into(),
            ));
        }
        if spawn.is_some() && self.from != AgentId::Captain {
            return Err(ToolError::InvalidArguments(
                "only the Captain may spawn".into(),
            ));
        }
        let reason = match reason_str {
            "Dispatch" => SteerReason::Dispatch,
            "Inject" => SteerReason::Inject,
            "Abort" => SteerReason::Abort,
            other => {
                return Err(ToolError::InvalidArguments(format!(
                    "unknown reason: {other}"
                )));
            }
        };

        // Address requirements per shape: member dispatch addresses by name;
        // Inject/Abort address a live target; run creation needs no address.
        if reason != SteerReason::Dispatch && agent_address.is_none() {
            return Err(ToolError::InvalidArguments(
                "'to.agent_address' is required for Inject and Abort".into(),
            ));
        }
        if spawn == Some(MEMBER_SPAWN) && agent_address.is_none() {
            return Err(ToolError::InvalidArguments(
                "'to.agent_address' is required when spawning TeamMember (the member name)".into(),
            ));
        }
        // Dispatch-only parameters are meaningless on Inject/Abort — a
        // silently ignored budget would run the child under a different
        // contract than asked.
        if reason != SteerReason::Dispatch
            && let Some(stray) = first_stray_dispatch_param(&params)
        {
            return Err(ToolError::InvalidArguments(format!(
                "`{stray}` only applies to Dispatch."
            )));
        }
        // The run dispatches label their card; the member arm carries the
        // prompt alone.
        if reason == SteerReason::Dispatch
            && spawn.is_some()
            && spawn != Some(MEMBER_SPAWN)
            && params.get("description").and_then(|v| v.as_str()).is_none()
        {
            return Err(ToolError::InvalidArguments(
                "'description' is required for run dispatches (the short task label)".into(),
            ));
        }

        let to_spec = ToSpec {
            agent_address: agent_address.unwrap_or_default().to_string(),
            spawn: spawn.map(String::from),
        };
        let payload = SteerPayload {
            text: prompt.to_string(),
        };
        let call = SteerCall {
            tool_call_id: tool_call_id.to_string(),
            signal,
            dispatch_params: params,
        };

        self.bus
            .steer(self.from.clone(), to_spec, reason, payload, call)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subagent::SubagentCatalog;

    fn bus() -> Arc<AgentBus> {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        AgentBus::new("thread-1".into(), tx)
    }

    fn to(addr: &str, spawn: Option<&str>) -> ToSpec {
        ToSpec {
            agent_address: addr.into(),
            spawn: spawn.map(String::from),
        }
    }

    fn payload(text: &str) -> SteerPayload {
        SteerPayload { text: text.into() }
    }

    fn call() -> SteerCall {
        SteerCall {
            tool_call_id: "call-1".into(),
            signal: CancellationToken::new(),
            dispatch_params: serde_json::json!({
                "description": "d",
                "prompt": "p",
            }),
        }
    }

    #[tokio::test]
    async fn subagent_cannot_spawn() {
        let bus = bus();
        let err = bus
            .steer(
                AgentId::Subagent("w1".into()),
                to("m", Some("TeamMember")),
                SteerReason::Dispatch,
                payload("x"),
                call(),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not allowed"), "{err}");
    }

    #[tokio::test]
    async fn captain_cannot_inject_self() {
        let bus = bus();
        let err = bus
            .steer(
                AgentId::Captain,
                to("Captain", None),
                SteerReason::Inject,
                payload("x"),
                call(),
            )
            .await
            .unwrap_err();
        // A reserved address is not a steer target: the refusal names the
        // address contract, not a permission wall.
        assert!(err.to_string().contains("no subagent run"), "{err}");
    }

    #[tokio::test]
    async fn abort_unknown_address_denied() {
        let bus = bus();
        let err = bus
            .steer(
                AgentId::Captain,
                to("ghost", None),
                SteerReason::Abort,
                payload("x"),
                call(),
            )
            .await
            .unwrap_err();
        // An unknown address has its own arm before the permission
        // fallthrough — the refusal names the address contract.
        assert!(err.to_string().contains("no subagent run"), "{err}");
    }

    /// Without an installed surface (no session assembly ran), a run
    /// dispatch fails loudly rather than silently doing nothing.
    #[tokio::test]
    async fn run_dispatch_without_a_surface_fails_loud() {
        let bus = bus();
        let err = bus
            .steer(
                AgentId::Captain,
                to("", Some("Explore")),
                SteerReason::Dispatch,
                payload("x"),
                call(),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no subagent surface"), "{err}");
    }

    /// An unknown definition name is refused with the discovery entry point
    /// named — the model picks `to.spawn` values from ListAgents' catalog.
    #[tokio::test]
    async fn unknown_definition_names_the_discovery_entry_point() {
        let bus = bus();
        let (notice_tx, _notice_rx) = tokio::sync::mpsc::unbounded_channel();
        bus.install_delegation(SubagentDelegation {
            runtime: manox_harness::subagent::SubagentRuntime::new(),
            observer: crate::subagent::SubagentRunObserver::new("thread-1".into(), notice_tx),
            dispatcher: test_dispatcher(),
            catalog: Arc::new(SubagentCatalog::new(vec![])),
        });
        let err = bus
            .steer(
                AgentId::Captain,
                to("", Some("Ghost")),
                SteerReason::Dispatch,
                payload("x"),
                call(),
            )
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("ListAgents"),
            "the refusal teaches the catalog: {err}"
        );
    }

    /// A continuation to an unknown run id is refused with the address
    /// lifecycle named.
    #[tokio::test]
    async fn continue_unknown_run_names_the_address_contract() {
        let bus = bus();
        let (notice_tx, _notice_rx) = tokio::sync::mpsc::unbounded_channel();
        bus.install_delegation(SubagentDelegation {
            runtime: manox_harness::subagent::SubagentRuntime::new(),
            observer: crate::subagent::SubagentRunObserver::new("thread-1".into(), notice_tx),
            dispatcher: test_dispatcher(),
            catalog: Arc::new(SubagentCatalog::new(vec![])),
        });
        let err = bus
            .steer(
                AgentId::Captain,
                to("sub-ghost", None),
                SteerReason::Dispatch,
                payload("x"),
                call(),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no subagent run"), "{err}");
    }

    /// A scripted provider for the run arms: full caps, records every
    /// start's continuation field, fires the host observer's lifecycle like
    /// the real spawn drive, and settles immediately (foreground create) or
    /// holds until aborted (the live-run arms). A CONTINUATION start always
    /// settles immediately — the reopened run has nothing left to hold on.
    struct ScriptedProvider {
        settle: bool,
        continuations: Mutex<Vec<Option<String>>>,
        observer: Mutex<Option<Arc<crate::subagent::SubagentRunObserver>>>,
    }

    impl ScriptedProvider {
        fn new(settle: bool) -> Arc<Self> {
            Arc::new(Self {
                settle,
                continuations: Mutex::new(Vec::new()),
                observer: Mutex::new(None),
            })
        }

        fn with_observer(
            self: &Arc<Self>,
            observer: Arc<crate::subagent::SubagentRunObserver>,
        ) -> Arc<Self> {
            *self.observer.lock().unwrap() = Some(observer);
            Arc::clone(self)
        }
    }

    #[async_trait::async_trait]
    impl manox_harness::subagent::SubagentProvider for ScriptedProvider {
        fn name(&self) -> &str {
            "scripted"
        }

        fn capabilities(&self) -> manox_harness::subagent::Capabilities {
            manox_harness::subagent::Capabilities {
                agent_options: true,
                output_schema: false,
                depth_limit: true,
                tool_filter: true,
                permissions: true,
                persona: true,
                max_turns: true,
            }
        }

        async fn start(
            &self,
            request: manox_harness::subagent::types::ResolvedStartRequest,
        ) -> Result<manox_harness::subagent::SubagentRun, manox_harness::subagent::SubagentError>
        {
            use manox_harness::subagent::RunObserver as _;
            self.continuations
                .lock()
                .unwrap()
                .push(request.inner.continuation_of.clone());
            let observer = self.observer.lock().unwrap().clone();
            if let Some(observer) = &observer {
                observer.on_start(&manox_harness::subagent::RunInfo {
                    run_id: request.run_id.clone(),
                    provider: self.name().to_string(),
                    label: request.inner.label.clone(),
                    kind: request.inner.kind.clone(),
                    model: None,
                    local: true,
                    budgets: request.inner.budgets,
                    tool_call_id: request.inner.tool_call_id.clone(),
                });
            }
            let cancel = request.inner.cancel.clone();
            let run_id = request.run_id.clone();
            let label = request.inner.label.clone();
            let settle = self.settle || request.inner.continuation_of.is_some();
            let settle_observer = observer.clone();
            Ok(manox_harness::subagent::SubagentRun::new(
                request.run_id,
                cancel.clone(),
                Box::pin(async move {
                    let result = if settle {
                        manox_harness::subagent::RunResult {
                            output: "the report".into(),
                            structured: None,
                            diagnostic: None,
                            stop_reason: manox_harness::subagent::StopReason::Completed,
                        }
                    } else {
                        cancel.cancelled().await;
                        manox_harness::subagent::RunResult {
                            output: String::new(),
                            structured: None,
                            diagnostic: None,
                            stop_reason: manox_harness::subagent::StopReason::Aborted,
                        }
                    };
                    if let Some(observer) = &settle_observer {
                        observer.on_settled(&manox_harness::subagent::RunEndInfo {
                            run_id,
                            provider: "scripted".to_string(),
                            label,
                            local: true,
                            stop_reason: result.stop_reason.clone(),
                            output: result.output.clone(),
                        });
                    }
                    result
                }),
            ))
        }
    }

    fn def_config(name: &str) -> manox_harness::subagent::SubagentDefConfig {
        manox_harness::subagent::SubagentDefConfig {
            provider: "scripted".into(),
            name: name.into(),
            capability: "read-only".into(),
            description: "d".into(),
            persona: "p".into(),
            default_tools: vec![],
            default_denied_tools: vec![],
            frontmatter_model_spec: None,
            config_model_spec: None,
            frontmatter_effort: None,
            frontmatter_isolation: None,
            frontmatter_background: false,
            frontmatter_max_turns: None,
            color: None,
            declared_permissions: Default::default(),
            max_depth: 1,
        }
    }

    fn install(
        bus: &AgentBus,
        provider: Arc<ScriptedProvider>,
        defs: Vec<manox_harness::subagent::SubagentDefConfig>,
    ) -> Arc<crate::subagent::SubagentRunObserver> {
        let observer = crate::subagent::SubagentRunObserver::new(
            "thread-1".into(),
            tokio::sync::mpsc::unbounded_channel().0,
        );
        let provider = provider.with_observer(Arc::clone(&observer));
        let runtime = manox_harness::subagent::SubagentRuntime::new();
        runtime.register_provider_permanent(provider);
        bus.install_delegation(SubagentDelegation {
            runtime,
            observer: Arc::clone(&observer),
            dispatcher: test_dispatcher(),
            catalog: Arc::new(SubagentCatalog::new(defs)),
        });
        observer
    }

    /// A dispatch run params object with the required label + task.
    fn dispatch_params(extra: serde_json::Value) -> SteerCall {
        let mut params = serde_json::json!({
            "description": "survey",
            "prompt": "do it",
        });
        if let (Some(base), Some(extra)) = (params.as_object_mut(), extra.as_object()) {
            for (k, v) in extra {
                base.insert(k.clone(), v.clone());
            }
        }
        SteerCall {
            tool_call_id: "call-1".into(),
            signal: CancellationToken::new(),
            dispatch_params: params,
        }
    }

    /// The create arm: Dispatch with `to.spawn` = a definition starts a run
    /// on the runtime and the ack carries the minted run id (the permanent
    /// address for Inject/Abort/continue).
    #[tokio::test]
    async fn dispatch_to_a_definition_creates_a_run_and_acks_its_id() {
        crate::runtime::init_hermetic_for_test();
        let bus = bus();
        install(&bus, ScriptedProvider::new(true), vec![def_config("Scout")]);
        let result = bus
            .steer(
                AgentId::Captain,
                to("", Some("Scout")),
                SteerReason::Dispatch,
                payload("do it"),
                dispatch_params(serde_json::json!({})),
            )
            .await
            .expect("the run starts");
        let envelope: serde_json::Value = serde_json::from_str(&text_of(&result)).unwrap();
        assert_eq!(envelope["kind"], "foreground", "{envelope}");
        assert_eq!(envelope["stop_reason"], "completed", "{envelope}");
        assert_eq!(envelope["output"], "the report", "{envelope}");
        let run_id = envelope["run_id"].as_str().unwrap().to_string();
        assert!(run_id.starts_with("sub-"), "the ack carries the run id");
        // The run settled, so it left the live table — and its settlement
        // armed the continuation address.
        assert!(bus.is_settled_run(&run_id));
    }

    /// The continue arm: Dispatch to a run id with no `to.spawn` reopens the
    /// same run — `continuation_of` rides to the provider, and the config
    /// resolves from the run's remembered definition.
    #[tokio::test]
    async fn dispatch_to_a_run_id_continues_it() {
        crate::runtime::init_hermetic_for_test();
        let bus = bus();
        let provider = ScriptedProvider::new(true);
        install(&bus, Arc::clone(&provider), vec![def_config("Scout")]);
        // The remembered-kind map fills through the observer, as production
        // does on every activation.
        let observer = bus.delegation().unwrap().observer.clone();
        use manox_harness::subagent::RunObserver as _;
        observer.on_start(&run_info("sub-known"));
        observer.on_settled(&settled_info("sub-known"));

        let result = bus
            .steer(
                AgentId::Captain,
                to("sub-known", None),
                SteerReason::Dispatch,
                payload("the follow-up"),
                dispatch_params(serde_json::json!({})),
            )
            .await
            .expect("the continuation starts");
        let envelope: serde_json::Value = serde_json::from_str(&text_of(&result)).unwrap();
        assert_eq!(envelope["kind"], "foreground", "{envelope}");
        assert_eq!(
            provider.continuations.lock().unwrap().as_slice(),
            &[Some("sub-known".to_string())],
            "the run id rode to the provider as the continuation"
        );
    }

    /// A continuation addresses a SETTLED run: while the run is live the
    /// dispatch is refused with the mid-run alternatives named; once it has
    /// settled, the same dispatch continues it.
    #[tokio::test]
    async fn dispatch_to_a_live_run_refuses_until_it_settles() {
        crate::runtime::init_hermetic_for_test();
        let bus = bus();
        let provider = ScriptedProvider::new(false);
        install(&bus, Arc::clone(&provider), vec![def_config("Scout")]);
        let result = bus
            .steer(
                AgentId::Captain,
                to("", Some("Scout")),
                SteerReason::Dispatch,
                payload("do it"),
                dispatch_params(serde_json::json!({"run_in_background": true})),
            )
            .await
            .expect("the background run starts");
        let envelope: serde_json::Value = serde_json::from_str(&text_of(&result)).unwrap();
        let run_id = envelope["run_id"].as_str().unwrap().to_string();
        assert!(bus.is_live_run(&run_id), "the run is live");

        let err = bus
            .steer(
                AgentId::Captain,
                to(&run_id, None),
                SteerReason::Dispatch,
                payload("the follow-up"),
                dispatch_params(serde_json::json!({})),
            )
            .await
            .unwrap_err();
        let text = err.to_string();
        assert!(
            text.contains("still live") && text.contains("Inject") && text.contains("Abort"),
            "the refusal coaches the mid-run alternatives: {err}"
        );
        assert_eq!(
            provider.continuations.lock().unwrap().as_slice(),
            [None],
            "only the create rode; the live continuation never reached the provider"
        );

        // Settle the run (Abort), then the continuation goes through.
        bus.steer(
            AgentId::Captain,
            to(&run_id, None),
            SteerReason::Abort,
            payload("ignored"),
            dispatch_params(serde_json::json!({})),
        )
        .await
        .expect("the abort lands");
        for _ in 0..200 {
            if !bus.is_live_run(&run_id) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(!bus.is_live_run(&run_id), "the run settled");
        bus.steer(
            AgentId::Captain,
            to(&run_id, None),
            SteerReason::Dispatch,
            payload("the follow-up"),
            dispatch_params(serde_json::json!({})),
        )
        .await
        .expect("the settled run continues");
        assert_eq!(
            provider.continuations.lock().unwrap().as_slice(),
            [None, Some(run_id.clone())],
            "the continuation rode to the provider"
        );
    }

    /// An explicit-`to.spawn` continuation must name the run's remembered
    /// definition: a mismatch is refused with the run's own kind named, the
    /// matching spawn rides, and the mismatch is forgiven only when the
    /// remembered kind is no longer registered (the takeover path).
    #[tokio::test]
    async fn explicit_spawn_continuation_must_match_the_remembered_kind() {
        crate::runtime::init_hermetic_for_test();
        let bus = bus();
        let provider = ScriptedProvider::new(true);
        install(
            &bus,
            Arc::clone(&provider),
            vec![def_config("Scout"), def_config("Sailor")],
        );
        let observer = bus.delegation().unwrap().observer.clone();
        use manox_harness::subagent::RunObserver as _;
        observer.on_start(&run_info("sub-known"));
        observer.on_settled(&settled_info("sub-known"));

        let err = bus
            .steer(
                AgentId::Captain,
                to("sub-known", Some("Sailor")),
                SteerReason::Dispatch,
                payload("the follow-up"),
                dispatch_params(serde_json::json!({})),
            )
            .await
            .unwrap_err();
        let text = err.to_string();
        assert!(
            text.contains("`Scout`") && text.contains("`Sailor`"),
            "the refusal names the run's own definition: {err}"
        );
        assert!(
            provider.continuations.lock().unwrap().is_empty(),
            "the mismatch never reached the provider"
        );

        // The matching spawn is the explicit shape of the same continuation.
        bus.steer(
            AgentId::Captain,
            to("sub-known", Some("Scout")),
            SteerReason::Dispatch,
            payload("the follow-up"),
            dispatch_params(serde_json::json!({})),
        )
        .await
        .expect("the matching spawn continues the run");
        assert_eq!(
            provider.continuations.lock().unwrap().as_slice(),
            &[Some("sub-known".to_string())],
        );

        // A run whose kind is no longer registered takes any current
        // definition — the takeover path the plain arm's refusal coaches.
        observer.on_start(&run_info_kind("sub-orphan", "Retired"));
        observer.on_settled(&settled_info("sub-orphan"));
        bus.steer(
            AgentId::Captain,
            to("sub-orphan", Some("Sailor")),
            SteerReason::Dispatch,
            payload("the follow-up"),
            dispatch_params(serde_json::json!({})),
        )
        .await
        .expect("the takeover continuation starts");
        assert_eq!(
            provider.continuations.lock().unwrap().as_slice(),
            &[
                Some("sub-known".to_string()),
                Some("sub-orphan".to_string())
            ],
        );
    }

    /// The frozen member arm carries the prompt alone: a stray dispatch
    /// parameter is refused loudly instead of silently ignored.
    #[tokio::test]
    async fn member_spawn_rejects_stray_dispatch_params() {
        let bus = bus();
        let err = bus
            .steer(
                AgentId::Captain,
                to("alice", Some("TeamMember")),
                SteerReason::Dispatch,
                payload("hello"),
                dispatch_params(serde_json::json!({"timeout_ms": 5_000})),
            )
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("timeout_ms") && err.to_string().contains("TeamMember"),
            "{err}"
        );
    }

    /// A run dispatch with no spawn and no address has its own copy — never
    /// a coaching message quoting an empty run id.
    #[tokio::test]
    async fn bare_dispatch_gets_the_address_copy() {
        let bus = bus();
        let err = bus
            .steer(
                AgentId::Captain,
                to("", None),
                SteerReason::Dispatch,
                payload("do it"),
                dispatch_params(serde_json::json!({})),
            )
            .await
            .unwrap_err();
        let text = err.to_string();
        assert!(
            text.contains("to.spawn") && text.contains("to.agent_address"),
            "{err}"
        );
        assert!(!text.contains("``"), "no empty address in the copy: {err}");
    }

    /// The guide arm routes by the runtime's liveness table: a live run's
    /// address reaches the guide arm, whose face-absent answer here is the
    /// settlement coaching — a scripted provider has no child session to
    /// carry a face; the engine-level e2e covers a real handle.
    #[tokio::test]
    async fn inject_routes_by_the_runtime_liveness_table() {
        crate::runtime::init_hermetic_for_test();
        let bus = bus();
        let observer = install(
            &bus,
            ScriptedProvider::new(false),
            vec![def_config("Scout")],
        );
        let result = bus
            .steer(
                AgentId::Captain,
                to("", Some("Scout")),
                SteerReason::Dispatch,
                payload("do it"),
                dispatch_params(serde_json::json!({"run_in_background": true})),
            )
            .await
            .expect("the background run starts");
        let envelope: serde_json::Value = serde_json::from_str(&text_of(&result)).unwrap();
        let run_id = envelope["run_id"].as_str().unwrap().to_string();
        assert!(bus.is_live_run(&run_id));
        assert!(observer.health_of(&run_id).is_some(), "a live row exists");

        let err = bus
            .steer(
                AgentId::Captain,
                to(&run_id, None),
                SteerReason::Inject,
                payload("narrow the scope"),
                dispatch_params(serde_json::json!({})),
            )
            .await
            .unwrap_err();
        let text = err.to_string();
        assert!(
            text.contains("no longer live"),
            "the guide arm owns a live-run address: {err}"
        );
    }

    /// The guide arm on a settled run refuses with the continuation contract
    /// named — never a silent no-op, never a parked answer.
    #[tokio::test]
    async fn inject_into_a_settled_run_coaches_the_dispatch_continuation() {
        crate::runtime::init_hermetic_for_test();
        let bus = bus();
        let observer = install(
            &bus,
            ScriptedProvider::new(false),
            vec![def_config("Scout")],
        );
        use manox_harness::subagent::RunObserver as _;
        observer.on_start(&run_info("sub-done"));
        observer.on_settled(&settled_info("sub-done"));
        let err = bus
            .steer(
                AgentId::Captain,
                to("sub-done", None),
                SteerReason::Inject,
                payload("x"),
                dispatch_params(serde_json::json!({})),
            )
            .await
            .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("settled"), "{err}");
        assert!(
            text.contains("Dispatch"),
            "the error teaches the continuation shape: {err}"
        );
    }

    /// The abort arm cancels a live run and acks the interruption.
    #[tokio::test]
    async fn abort_cancels_a_live_run() {
        crate::runtime::init_hermetic_for_test();
        let bus = bus();
        install(
            &bus,
            ScriptedProvider::new(false),
            vec![def_config("Scout")],
        );
        let result = bus
            .steer(
                AgentId::Captain,
                to("", Some("Scout")),
                SteerReason::Dispatch,
                payload("do it"),
                dispatch_params(serde_json::json!({"run_in_background": true})),
            )
            .await
            .expect("the background run starts");
        let envelope: serde_json::Value = serde_json::from_str(&text_of(&result)).unwrap();
        let run_id = envelope["run_id"].as_str().unwrap().to_string();
        assert!(bus.is_live_run(&run_id));

        let result = bus
            .steer(
                AgentId::Captain,
                to(&run_id, None),
                SteerReason::Abort,
                payload("ignored"),
                dispatch_params(serde_json::json!({})),
            )
            .await
            .expect("the abort lands");
        let envelope: serde_json::Value = serde_json::from_str(&text_of(&result)).unwrap();
        assert_eq!(envelope["interrupted"], true, "{envelope}");
        // The dispose token fired; the run leaves the table when its settle
        // watcher lands (asynchronous on purpose).
        for _ in 0..200 {
            if !bus.is_live_run(&run_id) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            !bus.is_live_run(&run_id),
            "the run settled out of the table"
        );
    }

    /// An Abort to a settled run refuses — only a live run can be aborted.
    #[tokio::test]
    async fn abort_a_settled_run_refuses() {
        crate::runtime::init_hermetic_for_test();
        let bus = bus();
        let observer = install(
            &bus,
            ScriptedProvider::new(false),
            vec![def_config("Scout")],
        );
        use manox_harness::subagent::RunObserver as _;
        observer.on_start(&run_info("sub-done"));
        observer.on_settled(&settled_info("sub-done"));
        let err = bus
            .steer(
                AgentId::Captain,
                to("sub-done", None),
                SteerReason::Abort,
                payload("x"),
                dispatch_params(serde_json::json!({})),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("already settled"), "{err}");
    }

    fn text_of(tool_result: &AgentToolResult) -> String {
        match &tool_result.content[0] {
            manox_harness::types::ContentBlock::Text { text, .. } => text.clone(),
            other => panic!("expected a text block: {other:?}"),
        }
    }

    fn run_info(run_id: &str) -> manox_harness::subagent::RunInfo {
        run_info_kind(run_id, "Scout")
    }

    fn run_info_kind(run_id: &str, kind: &str) -> manox_harness::subagent::RunInfo {
        manox_harness::subagent::RunInfo {
            run_id: run_id.to_string(),
            provider: "scripted".to_string(),
            label: "survey".to_string(),
            kind: kind.to_string(),
            model: None,
            local: true,
            budgets: manox_harness::subagent::Budgets::default(),
            tool_call_id: None,
        }
    }

    fn settled_info(run_id: &str) -> manox_harness::subagent::RunEndInfo {
        manox_harness::subagent::RunEndInfo {
            run_id: run_id.to_string(),
            provider: "scripted".to_string(),
            label: "survey".to_string(),
            local: true,
            stop_reason: manox_harness::subagent::StopReason::Completed,
            output: "done".to_string(),
        }
    }

    fn test_dispatcher() -> Arc<crate::subagent::SubagentDispatcher> {
        Arc::new(crate::subagent::SubagentDispatcher {
            observer: crate::subagent::SubagentRunObserver::new(
                "thread-1".into(),
                tokio::sync::mpsc::unbounded_channel().0,
            ),
            provider_registry: None,
            env: Arc::new(manox_harness::env::TokioExecutionEnv::new(
                std::path::PathBuf::from("/tmp"),
            )),
            cwd: std::path::PathBuf::from("/tmp"),
            parent_depth: 0,
            parent_session: Some("thread-1".into()),
        })
    }

    /// The user-cancel fan-out fires one `AbortMember` per spawned member
    /// (fire-and-forget: replies discarded), and nothing else.
    #[test]
    fn abort_all_members_fires_one_abort_per_spawned_member() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let bus = AgentBus::new("thread-1".into(), tx);
        bus.register_spawned_member_for_test("m1");
        bus.register_spawned_member_for_test("m2");
        bus.register_spawned_member_for_test("m3");

        bus.abort_all_members();

        let mut aborted: Vec<String> = Vec::new();
        while let Ok(notice) = rx.try_recv() {
            match notice {
                BackendNotice::BusRequest {
                    op: BusOp::AbortMember { thread_id },
                    ..
                } => aborted.push(thread_id),
                _ => panic!("unexpected non-AbortMember notice"),
            }
        }
        aborted.sort();
        assert_eq!(aborted, vec!["m1", "m2", "m3"]);
    }

    // ── the tool's execute face (schema + journaling) ────────────────────

    /// The serialized store tests' lock, as a guard (the store-backed
    /// journal test holds it for its whole body).
    fn steer_store_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::thread_store::store_test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn tool_context() -> manox_harness::tool::LocalToolContext {
        manox_harness::tool::LocalToolContext::new(
            Arc::new(manox_harness::env::TokioExecutionEnv::new(
                std::path::PathBuf::from("/tmp"),
            )),
            std::path::PathBuf::from("/tmp"),
            Arc::new(manox_harness::tool::ToolState::default()),
        )
    }

    /// An unknown top-level field is a typo'd contract, refused before any
    /// arm routes.
    #[tokio::test]
    async fn execute_rejects_an_unknown_top_level_field() {
        let tool = SteerTool::new(bus(), AgentId::Captain);
        let err = tool
            .execute(
                "call-1",
                serde_json::json!({
                    "to": {"spawn": "Scout"}, "reason": "Dispatch",
                    "prompt": "p", "description": "d", "budget_ms": 1,
                }),
                CancellationToken::new(),
                &tool_context(),
            )
            .await
            .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("unknown field"), "{err}");
        assert!(text.contains("budget_ms"), "{err}");
    }

    /// An unknown `to` field is refused the same way.
    #[tokio::test]
    async fn execute_rejects_an_unknown_to_field() {
        let tool = SteerTool::new(bus(), AgentId::Captain);
        let err = tool
            .execute(
                "call-1",
                serde_json::json!({
                    "to": {"agent_address": "", "spawn": "Scout", "who": "x"},
                    "reason": "Dispatch", "prompt": "p", "description": "d",
                }),
                CancellationToken::new(),
                &tool_context(),
            )
            .await
            .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("`to` has unknown field"), "{err}");
        assert!(text.contains("who"), "{err}");
    }

    /// Dispatch-only parameters on an Inject are refused — a silently
    /// ignored budget would run the child under a different contract.
    #[tokio::test]
    async fn execute_rejects_dispatch_only_params_on_an_inject() {
        let tool = SteerTool::new(bus(), AgentId::Captain);
        let err = tool
            .execute(
                "call-1",
                serde_json::json!({
                    "to": {"agent_address": "sub-1"}, "reason": "Inject",
                    "prompt": "x", "timeout_ms": 100,
                }),
                CancellationToken::new(),
                &tool_context(),
            )
            .await
            .unwrap_err();
        let text = err.to_string();
        assert!(
            text.contains("timeout_ms") && text.contains("Dispatch"),
            "{err}"
        );
    }

    /// A run dispatch without the `description` label is refused: the card
    /// and the child's creation label both need it.
    #[tokio::test]
    async fn execute_rejects_a_run_dispatch_without_a_description() {
        let tool = SteerTool::new(bus(), AgentId::Captain);
        let err = tool
            .execute(
                "call-1",
                serde_json::json!({
                    "to": {"spawn": "Scout"}, "reason": "Dispatch", "prompt": "p",
                }),
                CancellationToken::new(),
                &tool_context(),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("description"), "{err}");
    }

    /// `description` is only meaningful on run dispatches: an Inject without
    /// one passes the shape gate (the schema no longer demands the label on
    /// every call, and the model must not invent one to guide or abort).
    /// The Inject below still refuses — on the unknown ADDRESS, never a
    /// demanded label.
    #[tokio::test]
    async fn execute_accepts_an_inject_without_a_description() {
        let tool = SteerTool::new(bus(), AgentId::Captain);
        let err = tool
            .execute(
                "call-1",
                serde_json::json!({
                    "to": {"agent_address": "sub-1"}, "reason": "Inject", "prompt": "x",
                }),
                CancellationToken::new(),
                &tool_context(),
            )
            .await
            .unwrap_err();
        let text = err.to_string();
        assert!(
            !text.contains("'description'"),
            "the Inject is never asked for a label: {err}"
        );
        assert!(
            text.contains("no subagent run"),
            "the refusal is the address coach, past the shape gate: {err}"
        );
    }

    /// The execute-path Inject lands the durable `run/guided` row BEFORE the
    /// delivery attempt: even a delivery that bounces (no steer face here —
    /// a scripted provider has no child session) leaves the witness in the
    /// parent journal. A sync test driving a local runtime: the store lock
    /// guards the global override for the whole body, and a guard must not
    /// sit across an `await` in an async test.
    #[test]
    fn inject_journals_the_run_guided_row_before_the_delivery() {
        crate::runtime::init_hermetic_for_test();
        let _store_lock = steer_store_lock();
        crate::thread_store::init_for_test();
        let journal_dir = crate::thread_store::global_sessions_dir()
            .join("t")
            .join("thread-1");
        std::fs::create_dir_all(&journal_dir).unwrap();
        let journal = journal_dir.join(manox_harness::session::identity::JOURNAL_FILE);

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            // A real journal file (header included): the cold append opens
            // the storage, so an empty stub would bounce the row.
            manox_harness::session::jsonl::JsonlSessionStorage::create(
                &journal,
                manox_harness::session::jsonl::JsonlSessionMetadata {
                    id: "thread-1".into(),
                    cwd: "/tmp".into(),
                    created_at: chrono::Utc::now(),
                    parent_session_path: None,
                    metadata: None,
                },
            )
            .await
            .expect("the journal seeds");

            let bus = bus();
            install(
                &bus,
                ScriptedProvider::new(false),
                vec![def_config("Scout")],
            );
            let result = bus
                .steer(
                    AgentId::Captain,
                    to("", Some("Scout")),
                    SteerReason::Dispatch,
                    payload("do it"),
                    dispatch_params(serde_json::json!({"run_in_background": true})),
                )
                .await
                .expect("the background run starts");
            let envelope: serde_json::Value = serde_json::from_str(&text_of(&result)).unwrap();
            let run_id = envelope["run_id"].as_str().unwrap().to_string();
            assert!(bus.is_live_run(&run_id));

            // The Inject's delivery bounces (no steer face), but the durable
            // witness landed first.
            bus.steer(
                AgentId::Captain,
                to(&run_id, None),
                SteerReason::Inject,
                payload("narrow the scope"),
                dispatch_params(serde_json::json!({})),
            )
            .await
            .unwrap_err();
        });
        let landed = (0..200).any(|_| {
            let ok = std::fs::read_to_string(&journal)
                .is_ok_and(|content| content.contains("run/guided") && content.contains("sub-"));
            if ok {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
            false
        });
        assert!(
            landed,
            "the run/guided row is the durable witness: {}",
            std::fs::read_to_string(&journal).unwrap_or_default()
        );
        crate::thread_store::drop_for_test();
    }

    /// The `ListAgents` execute face carries the definition catalog — the
    /// entry point the Steer `to.spawn` values are picked from.
    #[tokio::test]
    async fn list_agents_execute_lists_the_definition_catalog() {
        let (notice_tx, _notice_rx) = tokio::sync::mpsc::unbounded_channel();
        let runtime = manox_harness::subagent::SubagentRuntime::new();
        let observer = crate::subagent::SubagentRunObserver::new("thread-1".into(), notice_tx);
        let tool = crate::subagent::ListAgentsTool::new(
            Arc::clone(&runtime),
            Arc::clone(&observer),
            Arc::new(SubagentCatalog::new(vec![def_config("Scout")])),
        );
        let result = tool
            .execute(
                "call-1",
                serde_json::json!({}),
                CancellationToken::new(),
                &tool_context(),
            )
            .await
            .expect("the list lands");
        let listed: serde_json::Value = serde_json::from_str(&text_of(&result)).unwrap();
        assert_eq!(listed["runs"], serde_json::json!([]), "{listed}");
        let definitions = listed["definitions"].as_array().expect("definitions ride");
        assert_eq!(definitions.len(), 1, "{listed}");
        assert_eq!(definitions[0]["name"], "Scout", "{listed}");
        assert_eq!(definitions[0]["capability"], "read-only", "{listed}");
    }
}
