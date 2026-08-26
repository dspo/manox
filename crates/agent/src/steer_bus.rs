//! Host-side `AgentBus` + `Steer` tool — the unified inter-agent messaging
//! and spawn mechanism. Routes `Steer` messages between `User`, the
//! singleton `Captain`, and dynamically-spawned `Subagent` instances. Sits
//! on the kernel's intra-session `steer` (`HarnessHandle::steer`). TS Pi
//! has no cross-session agent bus — this is a manox host extension.

use std::collections::{BTreeMap, HashSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use pi::harness::HarnessHandle;
use pi::tool::{AgentTool, AgentToolResult, ToolContext, ToolError};
use pi::types::{AgentMessage, ContentBlock};
use pi_extensions::agents::SubagentTool;
use pi_extensions::steer_bus::{
    AgentId, BusOp, DispatchBudgets, SteerPayload, SteerReason, ToSpec,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::background_task::{self, TaskKind, TaskStatus};
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

/// The smallest wall-clock budget a Dispatch may attach — below it a
/// subagent cannot even land one model turn, so the budget is an input
/// error rather than an enforceable limit.
pub const MIN_SUBAGENT_TIMEOUT_MS: u64 = 1_000;

// ── AgentBus ─────────────────────────────────────────────────────────────

/// A live in-thread subagent coroutine (transient, not a manox Thread).
/// The `handle` allows Inject/Abort; the `cancel` token propagates
/// parent-turn abort to the spawned task; the `watchdog` is the shared
/// health state the run task writes and the status surfaces read.
#[derive(Clone)]
pub struct LiveSubagent {
    pub handle: HarnessHandle,
    pub cancel: CancellationToken,
    /// Monotonic dispatch generation; a settled run only removes its own
    /// entry so a same-address re-dispatch is not orphaned by the old run.
    pub run: u64,
    pub spawn_type: String,
    pub watchdog: Arc<std::sync::Mutex<crate::subagent_watchdog::SubagentWatchdog>>,
    /// The dispatch's armed budgets (`None` per axis = unbounded), kept so
    /// status queries can report the remaining wall-clock budget.
    pub budgets: DispatchBudgets,
}

/// The host-side agent bus. One per thread (Captain or member). The
/// Captain's bus tracks live subagents + spawned member thread-ids.
pub struct AgentBus {
    owner_thread_id: String,
    notice_tx: mpsc::UnboundedSender<BackendNotice>,
    live_subagents: Mutex<BTreeMap<String, LiveSubagent>>,
    spawned_members: Mutex<HashSet<String>>,
    run_seq: AtomicU64,
    weak_self: Mutex<Weak<AgentBus>>,
    subagent_tool: Mutex<Option<Arc<SubagentTool>>>,
    tool_ctx: Mutex<Option<Arc<dyn ToolContext>>>,
    /// Wires `subagent_tool` on first dispatch for sessions assembled
    /// before a model resolved (resume-at-launch race); returns whether a
    /// live model was available to wire with.
    late_configure: Mutex<Option<Arc<dyn Fn() -> bool + Send + Sync>>>,
}

/// Poison-tolerant lock: a holder that panicked must not take every later
/// dispatch down with a secondary panic (which reads as a silent hang on
/// the caller's tool channel).
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
            live_subagents: Mutex::new(BTreeMap::new()),
            spawned_members: Mutex::new(HashSet::new()),
            run_seq: AtomicU64::new(0),
            weak_self: Mutex::new(Weak::new()),
            subagent_tool: Mutex::new(None),
            tool_ctx: Mutex::new(None),
            late_configure: Mutex::new(None),
        });
        *bus.weak_self.lock().unwrap() = Arc::downgrade(&bus);
        bus
    }

    pub fn set_subagent_tool(&self, tool: Arc<SubagentTool>) {
        *self.subagent_tool.lock().unwrap() = Some(tool);
    }

    pub fn set_tool_ctx(&self, ctx: Arc<dyn ToolContext>) {
        *self.tool_ctx.lock().unwrap() = Some(ctx);
    }

    pub fn set_late_configure(&self, configure: Arc<dyn Fn() -> bool + Send + Sync>) {
        *self.late_configure.lock().unwrap() = Some(configure);
    }

    /// The main Steer routing entry point.
    pub async fn steer(
        &self,
        from: AgentId,
        to: ToSpec,
        reason: SteerReason,
        payload: SteerPayload,
    ) -> Result<AgentToolResult, ToolError> {
        let addr = &to.agent_address;
        let spawn = to.spawn.as_deref();
        let isolation = to.isolation.as_deref();

        match (&from, &reason, spawn) {
            // ── Dispatch: spawn subagent (capability def name) ─────────
            (AgentId::Captain, SteerReason::Dispatch, Some(spawn_type))
                if spawn_type != "TeamMember" =>
            {
                for (name, budget) in [
                    ("timeout", to.timeout_ms),
                    ("idle_timeout", to.idle_timeout_ms),
                ] {
                    if budget.is_some_and(|t| t < MIN_SUBAGENT_TIMEOUT_MS) {
                        return Err(ToolError::InvalidArguments(format!(
                            "{name} must be at least {MIN_SUBAGENT_TIMEOUT_MS}ms"
                        )));
                    }
                }
                let budgets = DispatchBudgets {
                    timeout_ms: to.timeout_ms,
                    idle_timeout_ms: to.idle_timeout_ms,
                };
                self.dispatch_subagent(addr, spawn_type, isolation, budgets, &payload.text)
                    .await
            }

            // ── Dispatch: spawn TeamMember (real thread) ───────────────
            (AgentId::Captain, SteerReason::Dispatch, Some("TeamMember")) => {
                if to.timeout_ms.is_some() || to.idle_timeout_ms.is_some() {
                    return Err(ToolError::InvalidArguments(
                        "timeouts apply only to in-thread subagents, not TeamMember threads".into(),
                    ));
                }
                self.dispatch_member(addr, &payload.text).await
            }

            // ── Inject to existing subagent ─────────────────────────────
            (AgentId::Captain, SteerReason::Inject, None) if addr == "Captain" => Err(
                ToolError::InvalidArguments("cannot steer User or self".into()),
            ),
            (AgentId::Captain, SteerReason::Inject, None)
                if self.live_subagents.lock().unwrap().contains_key(addr) =>
            {
                let sub = self.live_subagents.lock().unwrap().get(addr).cloned();
                if let Some(live) = sub {
                    live.handle.steer(AgentMessage::user(payload.text.clone()));
                    return Ok(ack(addr, false, None));
                }
                Err(ToolError::ExecutionFailed(format!("agent {addr} gone")))
            }

            // ── Inject to member thread ─────────────────────────────────
            (AgentId::Captain, SteerReason::Inject, None) if self.is_member(addr) => self
                .bus_request(BusOp::InjectMember {
                    thread_id: addr.to_string(),
                    payload: payload.text.clone(),
                })
                .await
                .map(|_| ack(addr, false, None)),

            // ── Inject from subagent to parent Captain ──────────────────
            (AgentId::Subagent(_), SteerReason::Inject, None) if addr == "Captain" => {
                let _ = self.notice_tx.send(BackendNotice::SteerDelivered {
                    from: from.clone(),
                    reason: SteerReason::Inject,
                    payload,
                });
                Ok(ack(addr, false, None))
            }

            // ── Abort subagent ──────────────────────────────────────────
            // Cancel-only: the run task's select loop observes the token,
            // aborts the child session and settles silently (no SteerDelivered
            // here — Complete emission lives solely in the settle path, so an
            // explicit abort neither double-reports nor revives the Captain).
            (AgentId::Captain, SteerReason::Abort, None)
                if self.live_subagents.lock().unwrap().contains_key(addr) =>
            {
                let live = self.live_subagents.lock().unwrap().get(addr).cloned();
                if let Some(live) = live {
                    live.cancel.cancel();
                    live.handle.abort();
                    Ok(ack(addr, false, None))
                } else {
                    Err(ToolError::ExecutionFailed(format!("agent {addr} gone")))
                }
            }

            // ── Abort member thread ─────────────────────────────────────
            (AgentId::Captain, SteerReason::Abort, None) if self.is_member(addr) => self
                .bus_request(BusOp::AbortMember {
                    thread_id: addr.to_string(),
                })
                .await
                .map(|_| ack(addr, false, None)),

            // ── Permission denied ───────────────────────────────────────
            _ => Err(ToolError::InvalidArguments(format!(
                "steer not allowed: from={from:?} to={addr} reason={reason:?} spawn={spawn:?}"
            ))),
        }
    }

    /// Check if `addr` is a known member thread id.
    fn is_member(&self, addr: &str) -> bool {
        self.spawned_members.lock().unwrap().contains(addr)
    }

    /// Snapshot the live subagents' health for the `SubagentStatus` tool:
    /// one row per address with the watchdog's verdict and run stats.
    pub fn subagent_status(&self) -> Vec<serde_json::Value> {
        let now = std::time::Instant::now();
        let live = locked(&self.live_subagents);
        live.iter()
            .map(|(addr, sub)| {
                let wd = locked(&sub.watchdog);
                let elapsed = now.saturating_duration_since(wd.started_at());
                let mut row = serde_json::json!({
                    "address": addr,
                    "spawn_type": sub.spawn_type,
                    "health": wd.health_line(now),
                    "turns": wd.turns(),
                    "tool_calls": wd.tool_calls(),
                    "running_for_ms": elapsed.as_millis() as u64,
                });
                if let Some(activity) = wd.last_activity() {
                    row["last_activity"] = serde_json::json!(activity);
                }
                if let Some(budget) = sub.budgets.timeout_ms {
                    let remaining = Duration::from_millis(budget).saturating_sub(elapsed);
                    row["timeout_remaining_ms"] = serde_json::json!(remaining.as_millis() as u64);
                }
                row
            })
            .collect()
    }

    /// Dispatch a subagent coroutine: spawn session, register bg task,
    /// run in tokio, emit SteerDelivered on completion.
    ///
    /// The dispatch body must never poll on the caller's executor: it takes
    /// std-Mutex locks (bus slots, the engine's model slot via the late
    /// configurator), and on a cooperative executor a contended lock starves
    /// the whole executor — a silent forever-hang that even a timeout cannot
    /// surface (the timeout polls on the same starved executor). Bridge to
    /// the dedicated tokio runtime instead; the oneshot keeps the caller
    /// executor-agnostic, and a stuck or panicked dispatch task surfaces as
    /// a stage-named error (timeout fired / sender dropped).
    async fn dispatch_subagent(
        &self,
        addr: &str,
        spawn_type: &str,
        isolation: Option<&str>,
        budgets: DispatchBudgets,
        prompt: &str,
    ) -> Result<AgentToolResult, ToolError> {
        let stage = Arc::new(std::sync::Mutex::new("entry"));
        let bus = locked(&self.weak_self)
            .upgrade()
            .ok_or_else(|| ToolError::ExecutionFailed("bus gone".into()))?;
        let addr_s = addr.to_string();
        let spawn_s = spawn_type.to_string();
        let isolation_s = isolation.map(str::to_string);
        let prompt_s = prompt.to_string();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let stage2 = Arc::clone(&stage);
        crate::runtime::handle().spawn(async move {
            let stage_err = Arc::clone(&stage2);
            let result = tokio::time::timeout(
                Duration::from_secs(90),
                bus.dispatch_subagent_inner(
                    &addr_s,
                    &spawn_s,
                    isolation_s.as_deref(),
                    budgets,
                    &prompt_s,
                    stage2,
                ),
            )
            .await
            .unwrap_or_else(|_| {
                Err(ToolError::ExecutionFailed(format!(
                    "subagent dispatch timed out at stage `{}` (addr={addr_s})",
                    *locked(&stage_err)
                )))
            });
            let _ = tx.send(result);
        });
        rx.await.map_err(|_| {
            ToolError::ExecutionFailed(format!(
                "subagent dispatch task died at stage `{}` (addr={addr}) — panic in dispatch",
                *locked(&stage)
            ))
        })?
    }

    async fn dispatch_subagent_inner(
        &self,
        addr: &str,
        spawn_type: &str,
        isolation: Option<&str>,
        budgets: DispatchBudgets,
        prompt: &str,
        stage: Arc<std::sync::Mutex<&'static str>>,
    ) -> Result<AgentToolResult, ToolError> {
        let addr = addr.to_string();
        let spawn_type = spawn_type.to_string();
        let isolation = isolation.map(String::from);
        let prompt = prompt.to_string();
        *locked(&stage) = "subagent-tool";
        let subagent_tool = match locked(&self.subagent_tool).clone() {
            Some(tool) => tool,
            None => {
                *locked(&stage) = "late-configure";
                let configure = locked(&self.late_configure).clone();
                let wired = match configure {
                    Some(configure) => match catch_unwind(AssertUnwindSafe(|| configure())) {
                        Ok(wired) => wired,
                        Err(_) => {
                            tracing::error!("late subagent configure panicked (addr={addr})");
                            false
                        }
                    },
                    None => false,
                };
                if !wired {
                    return Err(ToolError::ExecutionFailed(
                        "subagent tool not configured".into(),
                    ));
                }
                locked(&self.subagent_tool).clone().ok_or_else(|| {
                    ToolError::ExecutionFailed("subagent tool not configured".into())
                })?
            }
        };
        tracing::info!("steer dispatch: subagent tool resolved (addr={addr})");
        *locked(&stage) = "tool-ctx";
        let tool_ctx = locked(&self.tool_ctx)
            .clone()
            .ok_or_else(|| ToolError::ExecutionFailed("tool context not configured".into()))?;

        // Create child SteerTool (limited: from=Subagent, Inject-only).
        *locked(&stage) = "bus-upgrade";
        let bus_arc = locked(&self.weak_self)
            .upgrade()
            .ok_or_else(|| ToolError::ExecutionFailed("bus gone".into()))?;
        let child_steer = Arc::new(SteerTool::new(bus_arc, AgentId::Subagent(addr.to_string())));

        // Reserved addresses would collide with routing: an address of
        // "Captain" would shadow the self-reject Inject arm; "User" is
        // reserved for symmetry with the spawn-side guard.
        if addr == "Captain" || addr == "User" {
            return Err(ToolError::InvalidArguments(format!(
                "cannot use reserved address {addr}"
            )));
        }
        // Existence check: re-dispatching a live address would overwrite the
        // first run and orphan it (un-Abort/Inject-able). Matches the schema's
        // "Error if address exists" contract.
        if locked(&self.live_subagents).contains_key(&addr) {
            return Err(ToolError::InvalidArguments(format!(
                "agent {addr} already exists"
            )));
        }
        let run = self.run_seq.fetch_add(1, Ordering::SeqCst);
        // Spawn the session.
        *locked(&stage) = "spawn-session";
        tracing::info!(
            "steer dispatch: spawning subagent session (addr={addr}, type={spawn_type})"
        );
        let (mut session, session_guard, worktree) = subagent_tool
            .spawn_subagent_session(
                &spawn_type,
                isolation.as_deref(),
                &*tool_ctx,
                vec![child_steer],
                Some(self.owner_thread_id.as_str()),
            )
            .await?;
        let handle = session.handle();

        // Register background task for the sidebar card.
        let task_cancel = CancellationToken::new();
        let description = first_line(&prompt).unwrap_or_else(|| format!("Subagent {spawn_type}"));
        let (task_id, task) = background_task::register(
            TaskKind::Subagent,
            self.owner_thread_id.clone(),
            description,
            task_cancel.clone(),
        );
        let _ = self.notice_tx.send(BackendNotice::Event(Box::new(
            ThreadEvent::BackgroundTaskUpdated {
                snapshot: task.snapshot(&task_id),
            },
        )));
        // Emit SubagentProgress so the "智能体" tab shows the subagent.
        let _ = self.notice_tx.send(BackendNotice::Event(Box::new(
            ThreadEvent::SubagentProgress {
                id: addr.clone(),
                subagent_type: spawn_type.clone(),
                tool_uses: 0,
                token_usage: crate::language_model::TokenUsage::default(),
                latest_activity: Some(
                    first_line(&prompt).unwrap_or_else(|| format!("Subagent {spawn_type}")),
                ),
                status: crate::thread::ToolCallStatus::Running,
                health: Some("starting".into()),
            },
        )));

        // Track in live_subagents for Inject/Abort and health queries.
        *locked(&stage) = "register-task";
        let watchdog = Arc::new(std::sync::Mutex::new(
            crate::subagent_watchdog::SubagentWatchdog::new(
                std::time::Instant::now(),
                budgets.idle_timeout_ms,
            ),
        ));
        locked(&self.live_subagents).insert(
            addr.to_string(),
            LiveSubagent {
                handle: handle.clone(),
                cancel: task_cancel.clone(),
                run,
                spawn_type: spawn_type.clone(),
                watchdog: Arc::clone(&watchdog),
                budgets,
            },
        );
        // Spawn the run task.
        let notice_tx = self.notice_tx.clone();
        let weak = locked(&self.weak_self).clone();
        *locked(&stage) = "spawn-run";
        let addr_clone = addr.clone();
        let label = addr.clone();
        let full_prompt = format!(
            "{prompt}\n\nWhen done, end your turn with a concise summary: \
             what you changed (files + intent), what you ran (commands + \
             outcomes), and the final result."
        );
        let tool_ctx2 = tool_ctx.clone();

        // The tempdir guard (present only when no host session dir is
        // injected) must outlive the spawned task: the session JSONL lives
        // inside it, and dropping it mid-run deletes the transcript.
        let run_cancel = task_cancel.clone();
        let run_handle = handle.clone();
        let run_watchdog = Arc::clone(&watchdog);
        let spawn_type2 = spawn_type.clone();
        crate::runtime::handle().spawn(async move {
            let _session_guard = session_guard; // hold the tempdir alive for the session lifetime
            // Bridge the child session's streamed events to the workspace so
            // the sub-agent panel + rail show live dynamics (the retired
            // JSON bridge's successor). The subscription must outlive the loop.
            let (ev_tx, mut ev_rx) =
                tokio::sync::mpsc::unbounded_channel::<pi::types::AgentEvent>();
            let _subscription = session.subscribe(Arc::new(move |event, _cancel| {
                let _ = ev_tx.send(event);
                Box::pin(async move {})
            }));
            // Salvaged inside the killing arms while the prompt borrow is
            // provably dead; the settle path only reads these `String`s.
            let mut timed_out_partial = String::new();
            let mut stalled_partial = String::new();
            let mut prompt_fut = Box::pin(session.prompt(&full_prompt));
            // Wall-clock deadline: an armed dispatch gets a real sleep; an
            // unbounded dispatch parks a never-ready branch in its place so
            // the loop shape stays uniform.
            let deadline = match budgets.timeout_ms {
                Some(ms) => {
                    futures::future::Either::Left(tokio::time::sleep(Duration::from_millis(ms)))
                }
                None => futures::future::Either::Right(std::future::pending::<()>()),
            };
            tokio::pin!(deadline);
            let mut watchdog_tick = tokio::time::interval(crate::subagent_watchdog::WATCHDOG_TICK);
            watchdog_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut timed_out = false;
            let mut stalled = false;
            let result = loop {
                tokio::select! {
                    r = prompt_fut.as_mut() => break r,
                    Some(event) = ev_rx.recv() => {
                        // Health folds first; a state change publishes one
                        // throttled progress event (the surfaces render the
                        // health line; child_events_of keeps the transcript
                        // activity as before).
                        let health_change = {
                            let mut wd = locked(&run_watchdog);
                            let now = std::time::Instant::now();
                            wd.observe(&event, now).then(|| wd.health_line(now))
                        };
                        if let Some(line) = health_change {
                            let _ = notice_tx.send(BackendNotice::Event(Box::new(
                                ThreadEvent::SubagentProgress {
                                    id: addr_clone.clone(),
                                    subagent_type: spawn_type2.clone(),
                                    tool_uses: 0,
                                    token_usage: crate::language_model::TokenUsage::default(),
                                    latest_activity: None,
                                    status: crate::thread::ToolCallStatus::Running,
                                    health: Some(line),
                                },
                            )));
                        }
                        for ev in crate::pi_engine::adapt::child_events_of(&addr_clone, &event) {
                            let _ = notice_tx.send(BackendNotice::Event(Box::new(ev)));
                        }
                    }
                    // Abort / TaskStop / thread-deletion cancel: stop the child
                    // instead of letting it burn tokens to natural completion.
                    _ = run_cancel.cancelled() => {
                        drop(prompt_fut);
                        run_handle.abort();
                        break Err(anyhow::anyhow!("aborted"));
                    }
                    // Wall-clock budget expired: settle as a delivering
                    // failure (not a silent abort) so the Captain sees the
                    // budget ran out and decides what next. A prompt that
                    // finished in the same instant the budget expired is a
                    // success, not a timeout — re-check before killing
                    // (select! picks a ready branch at random).
                    () = deadline.as_mut() => {
                        if let std::task::Poll::Ready(r) = futures::poll!(prompt_fut.as_mut()) {
                            break r;
                        }
                        timed_out = true;
                        drop(prompt_fut);
                        run_handle.abort();
                        timed_out_partial =
                            truncate_final(&extract_final_text(session.harness_messages()));
                        break Err(anyhow::anyhow!("timed out"));
                    }
                    // Health tick: report a stall once; enforce an armed
                    // idle budget through the same settle path as the
                    // wall-clock timeout.
                    _ = watchdog_tick.tick() => {
                        let outcome = locked(&run_watchdog).tick(std::time::Instant::now());
                        match outcome {
                            crate::subagent_watchdog::TickOutcome::Idle => {}
                            crate::subagent_watchdog::TickOutcome::ReportStall => {
                                let line = locked(&run_watchdog)
                                    .health_line(std::time::Instant::now());
                                let _ = notice_tx.send(BackendNotice::Event(Box::new(
                                    ThreadEvent::SubagentProgress {
                                        id: addr_clone.clone(),
                                        subagent_type: spawn_type2.clone(),
                                        tool_uses: 0,
                                        token_usage: crate::language_model::TokenUsage::default(),
                                        latest_activity: None,
                                        status: crate::thread::ToolCallStatus::Running,
                                        health: Some(line),
                                    },
                                )));
                            }
                            crate::subagent_watchdog::TickOutcome::EnforceStall => {
                                stalled = true;
                                drop(prompt_fut);
                                run_handle.abort();
                                stalled_partial =
                                    truncate_final(&extract_final_text(session.harness_messages()));
                                break Err(anyhow::anyhow!("stalled"));
                            }
                        }
                    }
                }
            };
            let aborted = run_cancel.is_cancelled();
            // Clean up the worktree; a kept (non-pristine) worktree's note must
            // ride the Complete payload so the caller can find the edits.
            let kept = match worktree {
                Some(wt) => wt.clean_up(&*tool_ctx2).await,
                None => None,
            };
            let kept_note = kept
                .as_ref()
                .map(|k| format!("\n\n[worktree kept: {k}]"))
                .unwrap_or_default();
            if aborted {
                task.set_terminal_status(TaskStatus::Stopped);
                let _ = notice_tx.send(BackendNotice::Event(Box::new(
                    ThreadEvent::BackgroundTaskUpdated {
                        snapshot: task.snapshot(&task_id),
                    },
                )));
                let _ = notice_tx.send(BackendNotice::Event(Box::new(
                    ThreadEvent::SubagentProgress {
                        id: addr_clone.clone(),
                        subagent_type: spawn_type.clone(),
                        tool_uses: 0,
                        token_usage: crate::language_model::TokenUsage::default(),
                        latest_activity: Some("aborted".into()),
                        status: crate::thread::ToolCallStatus::Cancelled,
                        health: None,
                    },
                )));
                // Silent settle on explicit abort: no SteerDelivered, so the
                // Captain is not revived for a cancellation it requested.
            } else if timed_out || stalled {
                let now = std::time::Instant::now();
                let (turns, tool_calls, last_activity, elapsed, idle) = {
                    let wd = locked(&run_watchdog);
                    (
                        wd.turns(),
                        wd.tool_calls(),
                        wd.last_activity().map(str::to_string),
                        now.saturating_duration_since(wd.started_at()),
                        wd.idle_for(now),
                    )
                };
                let (cause, delivery) = if stalled {
                    (
                        format!("stalled after {}ms idle", idle.as_millis()),
                        stalled_delivery(
                            idle,
                            budgets.idle_timeout_ms.unwrap_or_default(),
                            turns,
                            tool_calls,
                            last_activity.as_deref(),
                            &stalled_partial,
                            &kept_note,
                        ),
                    )
                } else {
                    let budget = budgets.timeout_ms.unwrap_or_default();
                    (
                        format!("timed out after {budget}ms"),
                        timed_out_delivery(
                            budget,
                            elapsed,
                            turns,
                            tool_calls,
                            last_activity.as_deref(),
                            &timed_out_partial,
                            &kept_note,
                        ),
                    )
                };
                task.set_failure_summary(cause.clone());
                task.set_terminal_status(TaskStatus::Failed);
                let _ = notice_tx.send(BackendNotice::Event(Box::new(
                    ThreadEvent::BackgroundTaskUpdated {
                        snapshot: task.snapshot(&task_id),
                    },
                )));
                let _ = notice_tx.send(BackendNotice::Event(Box::new(
                    ThreadEvent::SubagentProgress {
                        id: addr_clone.clone(),
                        subagent_type: spawn_type.clone(),
                        tool_uses: 0,
                        token_usage: crate::language_model::TokenUsage::default(),
                        latest_activity: Some(cause),
                        status: crate::thread::ToolCallStatus::Error,
                        health: None,
                    },
                )));
                let _ = notice_tx.send(BackendNotice::SteerDelivered {
                    from: AgentId::Subagent(label),
                    reason: SteerReason::Complete,
                    payload: SteerPayload { text: delivery },
                });
            } else {
                match result {
                    Ok(messages) => {
                        let content = format!(
                            "{}{kept_note}",
                            truncate_final(&extract_final_text(&messages))
                        );
                        task.set_terminal_status(TaskStatus::Completed);
                        let _ = notice_tx.send(BackendNotice::Event(Box::new(
                            ThreadEvent::BackgroundTaskUpdated {
                                snapshot: task.snapshot(&task_id),
                            },
                        )));
                        let _ = notice_tx.send(BackendNotice::Event(Box::new(
                            ThreadEvent::SubagentProgress {
                                id: addr_clone.clone(),
                                subagent_type: spawn_type.clone(),
                                tool_uses: 0,
                                token_usage: crate::language_model::TokenUsage::default(),
                                latest_activity: Some(content.clone()),
                                status: crate::thread::ToolCallStatus::Success,
                                health: None,
                            },
                        )));
                        if !content.is_empty() {
                            let _ = notice_tx.send(BackendNotice::SteerDelivered {
                                from: AgentId::Subagent(label),
                                reason: SteerReason::Complete,
                                payload: SteerPayload { text: content },
                            });
                        }
                    }
                    Err(e) => {
                        task.set_terminal_status(TaskStatus::Failed);
                        let _ = notice_tx.send(BackendNotice::Event(Box::new(
                            ThreadEvent::BackgroundTaskUpdated {
                                snapshot: task.snapshot(&task_id),
                            },
                        )));
                        let _ = notice_tx.send(BackendNotice::Event(Box::new(
                            ThreadEvent::SubagentProgress {
                                id: addr_clone.clone(),
                                subagent_type: spawn_type.clone(),
                                tool_uses: 0,
                                token_usage: crate::language_model::TokenUsage::default(),
                                latest_activity: Some(
                                    format!("failed: {e}").chars().take(80).collect(),
                                ),
                                status: crate::thread::ToolCallStatus::Error,
                                health: None,
                            },
                        )));
                        let _ = notice_tx.send(BackendNotice::SteerDelivered {
                            from: AgentId::Subagent(label),
                            reason: SteerReason::Complete,
                            payload: SteerPayload {
                                text: format!("{SUBAGENT_FAILED_DELIVERY_PREFIX}{e}{kept_note}"),
                            },
                        });
                    }
                }
            }
            // Remove only our own run (identity check) so an older settle can't
            // orphan a same-address re-dispatch.
            if let Some(bus) = weak.upgrade() {
                let mut map = bus.live_subagents.lock().unwrap();
                if map.get(&addr_clone).is_some_and(|l| l.run == run) {
                    map.remove(&addr_clone);
                }
            }
        });

        Ok(ack(&addr, true, None))
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
        self.spawned_members.lock().unwrap().insert(result.clone());
        Ok(ack(name, true, Some(result)))
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
        let members: Vec<String> = self
            .spawned_members
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect();
        for thread_id in members {
            let _ = self.notice_tx.send(BackendNotice::BusRequest {
                op: BusOp::AbortMember { thread_id },
                responder: None,
            });
        }
    }

    /// Test-only: register a member id as spawned (production inserts via
    /// the `dispatch_member` facade round trip).
    #[cfg(any(test, feature = "test-support"))]
    pub fn register_spawned_member_for_test(&self, thread_id: &str) {
        self.spawned_members
            .lock()
            .unwrap()
            .insert(thread_id.to_string());
    }
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

/// The first non-empty line of a prompt, for the background-task card title
/// and the restored sub-agent rail rows.
pub fn first_line(prompt: &str) -> Option<String> {
    prompt
        .lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .map(str::to_string)
}

/// Extract the final assistant text from a session's message list.
fn extract_final_text(messages: &[AgentMessage]) -> String {
    for msg in messages.iter().rev() {
        if let AgentMessage::Assistant { content, .. } = msg {
            let text: String = content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            if !text.is_empty() {
                return text;
            }
        }
    }
    String::new()
}

/// Cap a subagent's final summary before it rides the Complete payload into the
/// caller's next context window (the retired execute_inner truncated at the
/// same order of magnitude).
const FINAL_MAX_BYTES: usize = 128 * 1024;
const FINAL_MAX_LINES: usize = 2000;
fn truncate_final(text: &str) -> String {
    let by_lines: String = text
        .lines()
        .take(FINAL_MAX_LINES)
        .collect::<Vec<_>>()
        .join("\n");
    if by_lines.len() > FINAL_MAX_BYTES {
        by_lines.chars().take(FINAL_MAX_BYTES).collect()
    } else {
        by_lines
    }
}

/// Build the timeout report delivered to the parent when a subagent's
/// wall-clock budget expires: the budget, the observed run stats, and a
/// best-effort salvage of whatever the subagent had already produced.
fn timed_out_delivery(
    budget_ms: u64,
    elapsed: Duration,
    turns: u64,
    tool_calls: u64,
    last_activity: Option<&str>,
    partial: &str,
    kept_note: &str,
) -> String {
    let secs = elapsed.as_secs_f64();
    let mut delivery = format!(
        "{SUBAGENT_TIMED_OUT_DELIVERY_PREFIX}budget {budget_ms}ms exceeded after \
         {secs:.1}s ({turns} turns, {tool_calls} tool calls)"
    );
    if let Some(activity) = last_activity {
        delivery.push_str(&format!("; last activity: {activity}"));
    }
    if !partial.is_empty() {
        delivery.push_str(&format!("\n\nPartial result:\n{partial}"));
    }
    delivery.push_str(kept_note);
    delivery
}

/// Build the stalled report delivered to the parent when a subagent's idle
/// budget expires: the silent window, the observed run stats, and a
/// best-effort salvage of whatever the subagent had already produced.
fn stalled_delivery(
    idle: Duration,
    idle_budget_ms: u64,
    turns: u64,
    tool_calls: u64,
    last_activity: Option<&str>,
    partial: &str,
    kept_note: &str,
) -> String {
    let secs = idle.as_secs_f64();
    let mut delivery = format!(
        "{SUBAGENT_TIMED_OUT_DELIVERY_PREFIX}stalled — no activity for {secs:.1}s \
         (idle budget {idle_budget_ms}ms exceeded; {turns} turns, {tool_calls} tool calls)"
    );
    if let Some(activity) = last_activity {
        delivery.push_str(&format!("; last activity: {activity}"));
    }
    if !partial.is_empty() {
        delivery.push_str(&format!("\n\nPartial result:\n{partial}"));
    }
    delivery.push_str(kept_note);
    delivery
}

// ── SteerTool ────────────────────────────────────────────────────────────

/// The model-facing Steer tool — unified inter-agent messaging + spawn.
pub struct SteerTool {
    bus: Arc<AgentBus>,
    from: AgentId,
}

impl SteerTool {
    pub fn new(bus: Arc<AgentBus>, from: AgentId) -> Self {
        Self { bus, from }
    }
}

#[async_trait::async_trait]
impl AgentTool for SteerTool {
    fn name(&self) -> &str {
        "Steer"
    }

    fn description(&self) -> &str {
        "Send a typed message (Steer) to another agent. Use `to.agent_address` \
         to address a subagent (in-thread coroutine) or a TeamMember thread id; \
         set `to.spawn` to a capability def name (e.g. 'Sailor','Explore') to \
         create an in-thread subagent, or 'TeamMember' to create a real manox \
         Thread (process). Only the Captain may spawn. `reason` is Dispatch \
         (start a task), Inject (mid-run message), or Abort (cancel). Dispatch \
         accepts an optional `timeout` (wall-clock, ms, min 1000) and \
         `idle_timeout` (max silence with no tool running, ms, min 1000, \
         enforced at ~5s tick granularity): on expiry the subagent is \
         terminated and a report is delivered. Complete is harness-emitted \
         on subagent termination — not callable here."
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
                            "description": "Target address. For a subagent: the caller-chosen address (in-thread coroutine). For a TeamMember: the system thread id (returned in the spawn ack). Reserved: 'Captain', 'User'."
                        },
                        "spawn": {
                            "type": "string",
                            "description": "Optional. A capability def name (e.g. 'Sailor','Explore') creates an in-thread subagent coroutine; 'TeamMember' creates a real manox Thread (process: persisted, sidebar-visible, resumable, own Captain session + own bus). Only the Captain may set. Error if address exists. Cannot be 'Captain' or 'User'."
                        }
                    },
                    "required": ["agent_address"]
                },
                "reason": {
                    "type": "string",
                    "enum": ["Dispatch", "Inject", "Abort"],
                    "description": "Complete is harness-emitted on subagent termination, not callable here."
                },
                "prompt": {
                    "type": "string",
                    "description": "Payload: task (Dispatch), message (Inject). Abort ignores it."
                },
                "isolation": {
                    "type": "string",
                    "enum": ["worktree"],
                    "description": "Optional throwaway git worktree for a spawned subagent. Only with to.spawn = a capability def (not TeamMember)."
                },
                "timeout": {
                    "type": "integer",
                    "description": "Optional wall-clock budget in milliseconds (min 1000) for a spawned in-thread subagent. On expiry the subagent is terminated and a timeout report is delivered. Only with to.spawn = a capability def (not TeamMember)."
                },
                "idle_timeout": {
                    "type": "integer",
                    "description": "Optional idle budget in milliseconds (min 1000): the longest stretch with no activity and no tool running before a spawned in-thread subagent is terminated as stalled and a report is delivered. Enforcement granularity is ~5s (watchdog tick), not the exact millisecond value. Only with to.spawn = a capability def (not TeamMember)."
                }
            },
            "required": ["to", "reason", "prompt"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        _signal: CancellationToken,
        _ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        let to = params
            .get("to")
            .ok_or_else(|| ToolError::InvalidArguments("'to' is required".into()))?;
        let agent_address = to
            .get("agent_address")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArguments("'to.agent_address' is required".into()))?;
        let spawn = to.get("spawn").and_then(|v| v.as_str());
        let reason_str = params
            .get("reason")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArguments("'reason' is required".into()))?;
        let prompt = params
            .get("prompt")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArguments("'prompt' is required".into()))?;
        let isolation = params.get("isolation").and_then(|v| v.as_str());
        let timeout = params.get("timeout").and_then(|v| v.as_u64());
        let idle_timeout = params.get("idle_timeout").and_then(|v| v.as_u64());

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

        let to_spec = ToSpec {
            agent_address: agent_address.to_string(),
            spawn: spawn.map(String::from),
            isolation: isolation.map(String::from),
            timeout_ms: timeout,
            idle_timeout_ms: idle_timeout,
        };
        let reason = match reason_str {
            "Dispatch" => SteerReason::Dispatch,
            "Inject" => SteerReason::Inject,
            "Abort" => SteerReason::Abort,
            _ => {
                return Err(ToolError::InvalidArguments(format!(
                    "unknown reason: {reason_str}"
                )));
            }
        };
        let payload = SteerPayload {
            text: prompt.to_string(),
        };

        self.bus
            .steer(self.from.clone(), to_spec, reason, payload)
            .await
    }
}

/// The model-facing subagent health query — the watchdog's pull surface.
/// Read-only and approval-free: it observes, never mutates.
pub struct SubagentStatusTool {
    bus: Arc<AgentBus>,
}

impl SubagentStatusTool {
    pub fn new(bus: Arc<AgentBus>) -> Self {
        Self { bus }
    }
}

#[async_trait::async_trait]
impl AgentTool for SubagentStatusTool {
    fn name(&self) -> &str {
        "SubagentStatus"
    }

    fn description(&self) -> &str {
        "Report the health of this thread's live subagents: state (working, \
         tool running, stalled, looping), turns, tool calls, running time, \
         last activity, and remaining timeout budget. Check it when a \
         subagent seems slow or silent before deciding to Inject, Abort, \
         or re-dispatch — do not guess."
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn requires_approval(&self, _params: &serde_json::Value) -> bool {
        false
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        _params: serde_json::Value,
        _signal: CancellationToken,
        _ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        let rows = self.bus.subagent_status();
        if rows.is_empty() {
            return Ok(AgentToolResult::text("no live subagents".to_string()));
        }
        Ok(AgentToolResult::text(
            serde_json::to_string_pretty(&rows)
                .unwrap_or_else(|_| "subagent status unavailable".to_string()),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bus() -> Arc<AgentBus> {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        AgentBus::new("thread-1".into(), tx)
    }

    fn to(addr: &str, spawn: Option<&str>) -> ToSpec {
        ToSpec {
            agent_address: addr.into(),
            spawn: spawn.map(String::from),
            isolation: None,
            timeout_ms: None,
            idle_timeout_ms: None,
        }
    }

    fn payload(text: &str) -> SteerPayload {
        SteerPayload { text: text.into() }
    }

    #[test]
    fn truncate_final_caps_lines_and_bytes() {
        let many_lines: String = (0..2500)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = truncate_final(&many_lines);
        assert_eq!(out.lines().count(), FINAL_MAX_LINES);

        let big = "x".repeat(FINAL_MAX_BYTES + 10);
        assert!(truncate_final(&big).len() <= FINAL_MAX_BYTES);
    }

    #[test]
    fn first_line_skips_blank_lead() {
        assert_eq!(
            first_line("\n  \n  do the thing\nmore"),
            Some("do the thing".into())
        );
        assert_eq!(first_line("   "), None);
    }

    #[tokio::test]
    async fn subagent_cannot_spawn() {
        let bus = bus();
        let err = bus
            .steer(
                AgentId::Subagent("w1".into()),
                to("w2", Some("Sailor")),
                SteerReason::Dispatch,
                payload("x"),
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
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cannot steer"), "{err}");
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
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not allowed"), "{err}");
    }

    #[tokio::test]
    async fn dispatch_timeout_below_minimum_rejected() {
        let bus = bus();
        let mut spec = to("w", Some("Explore"));
        spec.timeout_ms = Some(500);
        let err = bus
            .steer(AgentId::Captain, spec, SteerReason::Dispatch, payload("x"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("at least"), "{err}");
    }

    #[tokio::test]
    async fn team_member_dispatch_with_timeout_rejected() {
        let bus = bus();
        let mut spec = to("m", Some("TeamMember"));
        spec.timeout_ms = Some(60_000);
        let err = bus
            .steer(AgentId::Captain, spec, SteerReason::Dispatch, payload("x"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("TeamMember"), "{err}");
    }

    #[tokio::test]
    async fn dispatch_idle_timeout_below_minimum_rejected() {
        let bus = bus();
        let mut spec = to("w", Some("Sailor"));
        spec.idle_timeout_ms = Some(999);
        let err = bus
            .steer(AgentId::Captain, spec, SteerReason::Dispatch, payload("x"))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("idle_timeout") && err.to_string().contains("at least"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn subagent_status_is_empty_without_live_runs() {
        let bus = bus();
        assert!(bus.subagent_status().is_empty());
    }

    #[test]
    fn timed_out_delivery_names_budget_stats_and_partial() {
        let delivery = timed_out_delivery(
            60_000,
            Duration::from_millis(61_234),
            3,
            7,
            Some("Read path=src/a.rs"),
            "half an answer",
            "",
        );
        assert!(
            delivery.starts_with(SUBAGENT_TIMED_OUT_DELIVERY_PREFIX),
            "{delivery}"
        );
        assert!(delivery.contains("budget 60000ms"), "{delivery}");
        assert!(delivery.contains("(3 turns, 7 tool calls)"), "{delivery}");
        assert!(
            delivery.contains("last activity: Read path=src/a.rs"),
            "{delivery}"
        );
        assert!(
            delivery.contains("Partial result:\nhalf an answer"),
            "{delivery}"
        );
    }

    #[test]
    fn timed_out_delivery_omits_absent_fields_and_keeps_worktree_note() {
        let delivery = timed_out_delivery(
            1_000,
            Duration::from_millis(1_000),
            0,
            0,
            None,
            "",
            "\n\n[worktree kept: branch=b, path=/tmp/wt]",
        );
        assert!(!delivery.contains("last activity"), "{delivery}");
        assert!(!delivery.contains("Partial result"), "{delivery}");
        assert!(
            delivery.ends_with("[worktree kept: branch=b, path=/tmp/wt]"),
            "{delivery}"
        );
    }

    #[test]
    fn stalled_delivery_names_idle_window_and_stats() {
        let delivery = stalled_delivery(
            Duration::from_millis(31_500),
            30_000,
            2,
            4,
            Some("Grep pattern=foo"),
            "",
            "",
        );
        assert!(
            delivery.starts_with(SUBAGENT_TIMED_OUT_DELIVERY_PREFIX),
            "{delivery}"
        );
        assert!(
            delivery.contains("stalled — no activity for 31.5s"),
            "{delivery}"
        );
        assert!(delivery.contains("idle budget 30000ms"), "{delivery}");
        assert!(delivery.contains("2 turns, 4 tool calls"), "{delivery}");
        assert!(
            delivery.contains("last activity: Grep pattern=foo"),
            "{delivery}"
        );
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
}
