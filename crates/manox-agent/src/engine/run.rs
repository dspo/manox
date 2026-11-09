//! Engine run plumbing: driving a run to settlement, goal chaining,
//! steering resume, live-history sync, and harness subscriptions.

use super::*;

use futures::FutureExt;

/// Land one live journal row inside the select. The writer serializes
/// appends, so awaiting the reply here cannot deadlock the run branch.
///
/// A permanent loss (K4, L3) is fail-loud: the state the row describes
/// already took effect, so the first loss records itself durably, notifies
/// the facade, and cancels the turn; later rows of the same dead-storage run
/// park for the settle/idle drain without re-signaling (one notice, one
/// abort — the turn is already converging). Returns whether this row's loss
/// cancelled the turn.
#[allow(clippy::too_many_arguments)] // the K4 tail needs every sink it reports to
async fn land_live_row(
    appender: &Arc<JournalAppender>,
    kind: String,
    payload: serde_json::Value,
    state: &Arc<EngineState>,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
    handle: &manox_harness::harness::HarnessHandle,
    loss_signaled: &mut bool,
) -> bool {
    let Err(err) = append_typed_resilient(appender, &kind, payload.clone()).await else {
        return false;
    };
    if *loss_signaled {
        state.pending_journal.lock().unwrap().push((kind, payload));
        return false;
    }
    if let Some(row) = record_journal_loss(appender, &kind, &err).await {
        state.pending_journal.lock().unwrap().push(row);
    }
    if kind == "error" {
        return false;
    }
    *loss_signaled = true;
    send_notice(
        notice_tx,
        BackendNotice::Event(Box::new(ThreadEvent::Error(anyhow::anyhow!(
            "journal append permanently failed for `{kind}`: {err:#}; cancelling the turn"
        )))),
        "journal-loss turn cancellation",
    );
    handle.abort();
    true
}

/// What one supervised run left behind.
pub(super) struct DriveOutcome {
    pub(super) result: anyhow::Result<Vec<AgentMessage>>,
    /// An abort was requested (a user/system cancel, or the journal-loss
    /// cancel `land_live_row` raises).
    pub(super) abort_requested: bool,
    /// The run future panicked; the payload's message.
    pub(super) panic: Option<String>,
    /// The abort grace expired and the run future was dropped without
    /// resolving — the harness may be mid-state.
    pub(super) abandoned: bool,
}

/// The readable form of a panic payload.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "the run panicked with a non-string payload".to_string()
    }
}

/// Fold one completed (or caught) run poll into its outcome.
fn finish_run(
    polled: Result<anyhow::Result<Vec<AgentMessage>>, Box<dyn std::any::Any + Send>>,
    abort_requested: bool,
) -> DriveOutcome {
    match polled {
        Ok(result) => DriveOutcome {
            result,
            abort_requested,
            panic: None,
            abandoned: false,
        },
        Err(payload) => {
            let message = panic_message(payload.as_ref());
            tracing::error!(panic = %message, "the run future panicked; closing the turn as failed");
            DriveOutcome {
                result: Err(anyhow::anyhow!("{message}")),
                abort_requested,
                panic: Some(message),
                abandoned: false,
            }
        }
    }
}

/// Drive one session run to completion while still servicing mid-run
/// commands (abort/steer/cancel/shutdown) through the session handle.
/// Shared by every turn entry point (see `run_turn`).
///
/// The run future borrows `&mut AgentSession`, so it cannot be spawned: it is
/// polled here, wrapped once in `catch_unwind` so a panic becomes a normal
/// `Failed { cause: Panic }` close instead of an unwind through the actor. An
/// abort gets [`ABORT_GRACE`] to resolve on its own; a future that outlives it
/// is dropped and the turn closes `Aborted` with the session degraded.
///
/// While the run is in flight, a periodic tick refreshes the engine's
/// history mirror from the live transcript (`LiveHistory` notice) so a
/// thread switched back to mid-turn rebuilds from current progress.
#[allow(clippy::too_many_arguments)] // drive plumbing: each input is a distinct sink
pub(super) async fn drive_run<F>(
    run: F,
    handle: &manox_harness::harness::HarnessHandle,
    cmd_rx: &mut mpsc::UnboundedReceiver<SessionCmd>,
    turn: &mut TurnCommit,
    shutdown_after_run: &mut bool,
    live: Arc<Mutex<LiveTranscript>>,
    state: &Arc<EngineState>,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
    harness_model: &mut HarnessModel,
    sessions_dir: &Path,
    session_path: &Path,
) -> DriveOutcome
where
    F: std::future::Future<Output = anyhow::Result<Vec<AgentMessage>>>,
{
    // One wrap, once: the panic verdict is the turn's close, not an unwind.
    let run = std::panic::AssertUnwindSafe(run).catch_unwind();
    tokio::pin!(run);
    let appender = Arc::clone(turn.appender());
    // The journal writer serializes appends: this select may await an append
    // reply directly. The writer is a separate task that owns the write
    // order, and no caller holds a lock across file I/O, so a select handler
    // waiting on an append can never block the run branch from making
    // progress — the run's own appends are just more commands in the same
    // FIFO queue.
    let mut loss_signaled = false;
    let mut abort_requested = false;
    let mut channel_open = true;
    let mut live_ticker = tokio::time::interval(LIVE_HISTORY_TICK);
    live_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first tick completes immediately; consume it so the mirror is not
    // re-synced at run start (the settle/ready path already mirrored).
    live_ticker.tick().await;
    // Stall watchdog: a turn whose journal tail stops moving while the run
    // future stays pending has durable progress stuck behind something. The
    // warn carries the frozen cursor so the log bracket around the stall is
    // unmissable; it repeats every 2 minutes.
    let mut watchdog_cursor: Option<u64> = None;
    let mut watchdog_last_move = std::time::Instant::now();
    let appender_for_watchdog = Arc::clone(&appender);
    // Armed by the abort arm (a far deadline keeps the branch inert before
    // that): an abort that the run does not act on within the grace costs the
    // future its life.
    let grace = tokio::time::sleep_until(
        tokio::time::Instant::now() + std::time::Duration::from_secs(24 * 3600),
    );
    tokio::pin!(grace);
    loop {
        if !channel_open {
            break finish_run(run.await, abort_requested);
        }
        tokio::select! {
            _ = &mut grace => {
                // The run did not resolve within the grace: drop it. Closure
                // is still guaranteed (the commit runs below); only the
                // harness's own state is now suspect, which the degraded flag
                // records.
                tracing::error!(
                    session = ?session_path,
                    grace_ms = state.abort_grace().as_millis() as u64,
                    "the run future outlived the abort grace; dropping it"
                );
                break DriveOutcome {
                    result: Err(anyhow::anyhow!(
                        "the run was abandoned after the abort grace expired"
                    )),
                    abort_requested: true,
                    panic: None,
                    abandoned: true,
                };
            }
            _ = live_ticker.tick() => {
                if sync_live_history(&live, state) {
                    send_notice(notice_tx, BackendNotice::LiveHistory, "live history refresh");
                }
                let cursor = appender_for_watchdog.storage().journal_cursor().await;
                if watchdog_cursor != Some(cursor) {
                    watchdog_cursor = Some(cursor);
                    watchdog_last_move = std::time::Instant::now();
                } else if watchdog_last_move.elapsed()
                    > std::time::Duration::from_secs(120)
                {
                    tracing::warn!(
                        session = ?session_path,
                        cursor,
                        elapsed_secs = watchdog_last_move.elapsed().as_secs(),
                        "turn stall watchdog: the journal tail has not moved while the run stays pending"
                    );
                    watchdog_last_move = std::time::Instant::now();
                }
            }
            maybe_cmd = cmd_rx.recv() => match maybe_cmd {
                Some(SessionCmd::Abort) => {
                    abort_requested = true;
                    handle.abort();
                    grace
                        .as_mut()
                        .reset(tokio::time::Instant::now() + state.abort_grace());
                }
                Some(SessionCmd::Steer { id, text, images }) => {
                    // S4: enqueue under the command id (== the client
                    // message id post-S3) so a later `cancel_steer(&id)` can
                    // retract this exact steer from the kernel queue if the
                    // run aborts before draining it. S3: the same id rides the
                    // injected `user` row as its durable identity.
                    handle.steer_with_id(steer_message(id.clone(), text, images), id.clone());
                    turn.record_steer(id);
                }
                Some(SessionCmd::CancelSteer(id)) => {
                    handle.cancel_steer(&id);
                }
                Some(SessionCmd::Shutdown) => *shutdown_after_run = true,
                Some(SessionCmd::SetModel(new_model)) => {
                    // Mid-run switch: the harness handle queues it for the
                    // next turn boundary (the kernel's mid-run `set_model`
                    // path), where the model_change entry persists. The
                    // mirrors follow the handle's verdict, not the request: a
                    // model the fixed stream refuses leaves every mirror on
                    // the model the run still serves, and the model already in
                    // play is never re-queued. A refusal is loud — the
                    // dispatch was echoed as accepted, so a silent drop here
                    // would strand the client's chip on a model that never
                    // landed (unreachable with the registry resolver
                    // installed; the belt is for the fixed-stream shape).
                    if *harness_model != new_model {
                        if handle.set_model(new_model.clone()) {
                            *state.model.lock().unwrap() = Some(new_model.clone());
                            *harness_model = new_model;
                        } else {
                            tracing::warn!(
                                model = %new_model.id,
                                "mid-run set_model refused; the run keeps the current model"
                            );
                        }
                    }
                }
                Some(SessionCmd::SetThinkingLevel(level)) => {
                    // Mid-run switch: the change lands on the runtime snapshot
                    // at once and governs the next request. The row decision
                    // is the same durable baseline the idle arm diffs against
                    // — a restate of the journal's carried level lands no row
                    // — and a real switch journals through the shared landing
                    // vocabulary. Awaiting the writer here is safe: it is a
                    // separate task and no lock spans its file I/O.
                    handle.set_thinking_level(level.clone());
                    let wire = thinking_level_wire(&level);
                    if state.journaled_thinking_level.lock().unwrap().as_deref()
                        != Some(wire.as_str())
                    {
                        if land_live_row(
                            &appender,
                            "thinking_level_change".into(),
                            thinking_level_row_payload(&wire),
                            state,
                            notice_tx,
                            handle,
                            &mut loss_signaled,
                        )
                        .await
                        {
                            abort_requested = true;
                            grace
                                .as_mut()
                                .reset(tokio::time::Instant::now() + state.abort_grace());
                        }
                        *state.journaled_thinking_level.lock().unwrap() = Some(wire);
                    }
                }
                Some(SessionCmd::PersistGeneratedTitle {
                    session_path: target,
                    title,
                }) if target == session_path => {
                    if let Err(error) = persist_title(sessions_dir, &target, title.clone()).await {
                        tracing::warn!(%error, "failed to persist Title agent result");
                    } else {
                        send_notice(notice_tx, BackendNotice::SessionListDirty, "session list dirty");
                        // The facade mirrors the persisted title so the
                        // title bar tracks the sidebar without a reload.
                        send_notice(notice_tx, BackendNotice::Event(Box::new(
                            ThreadEvent::TitleChanged { title },
                        )), "title changed notice");
                    }
                }
                Some(SessionCmd::AppendUiNote(record)) => {
                    // A mid-run switch-back must already show the card:
                    // merge it into the mirror now and park persistence for
                    // the projection (the actor owns the session; a second
                    // writer could fork the leaf cursor).
                    mirror_ui_note(state, record.clone());
                    state.pending_ui_notes.lock().unwrap().push(record);
                }
                Some(SessionCmd::AppendJournal { kind, payload }) => {
                    // Rows append LIVE through the shared session handle in
                    // emission order: the writer serializes them against the
                    // run's own appends, and awaiting its reply here cannot
                    // block the run branch (the writer is a separate task).
                    // Parking for settle would hide subagent/retry/background
                    // rows for the whole run (L5).
                    if land_live_row(
                        &appender,
                        kind,
                        payload,
                        state,
                        notice_tx,
                        handle,
                        &mut loss_signaled,
                    )
                    .await
                    {
                        abort_requested = true;
                        grace
                            .as_mut()
                            .reset(tokio::time::Instant::now() + state.abort_grace());
                    }
                }
                Some(SessionCmd::JournalSnapshot { reply }) => {
                    // Answer live off the storage (same read face as the
                    // idle loop): parking froze GetConversationInfo and
                    // PageHistory — the Q face AND the follow streams' gap
                    // repair — for the entire duration of a running turn.
                    reply_journal_snapshot(&appender, reply).await;
                }
                Some(SessionCmd::SetBrowserSuite { suite, enable }) => {
                    // The run owns the session; park the toggle so the idle
                    // loop applies it right after settle (P2: a mid-run click
                    // must not be silently dropped).
                    *state.pending_browser_suite.lock().unwrap() = Some((suite, enable));
                }
                Some(cmd) => { // not serviceable mid-run; park for post-settle
                    state.pending_session_cmds.lock().unwrap().push(cmd);
                }
                None => {
                    // Facade dropped mid-run: abort, settle, exit.
                    channel_open = false;
                    *shutdown_after_run = true;
                    if !abort_requested {
                        abort_requested = true;
                        handle.abort();
                        grace
                            .as_mut()
                            .reset(tokio::time::Instant::now() + state.abort_grace());
                    }
                }
            },
            res = &mut run => break finish_run(res, abort_requested),
        }
    }
}

/// How a turn ended, derived from the run result and the drive loop's own
/// verdict. This — never `Result::is_err` — is what the durable close
/// journals and what the `Settled` notice reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RunOutcome {
    /// The run finished and its terminal assistant did not fail.
    Completed,
    /// The turn failed. `terminal` is the message a human reads (the run
    /// error, the terminal assistant's `error_message`, or the panic
    /// payload); `cause` names which of those it was.
    Failed {
        terminal: Option<String>,
        cause: FailureCause,
    },
    /// The turn was aborted (user cancel, journal-loss cancel) and closed
    /// cancelled rather than failed.
    Aborted,
}

/// Why a turn failed: the provider/loop reported a terminal error, the run
/// future panicked, or the run was lost (it errored while already aborted).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FailureCause {
    Terminal,
    Panic,
    Lost,
}

impl RunOutcome {
    /// The `failed` flag the durable `turn_finish` row carries.
    pub(super) fn failed(&self) -> bool {
        matches!(self, RunOutcome::Failed { .. })
    }

    /// The `cancelled` flag the durable `turn_finish` row carries.
    pub(super) fn cancelled(&self) -> bool {
        matches!(self, RunOutcome::Aborted)
    }
}

/// The run result's own failure story. An `Err` is one; so is an `Ok` run
/// whose terminal assistant carries `stop_reason: Error` — the loop returned
/// Ok because the partial assistant and its synthesized tool results are
/// durable, not because the turn succeeded.
pub(super) fn terminal_error(result: &anyhow::Result<Vec<AgentMessage>>) -> Option<String> {
    match result {
        Err(err) => Some(format!("{err:#}")),
        Ok(messages) => {
            let terminal = messages
                .iter()
                .rev()
                .find(|message| matches!(message, AgentMessage::Assistant { .. }))?;
            match terminal {
                AgentMessage::Assistant {
                    stop_reason,
                    error_message,
                    ..
                } if *stop_reason == Some(manox_harness::types::StopReason::Error) => Some(
                    error_message
                        .clone()
                        .filter(|text| !text.trim().is_empty())
                        .unwrap_or_else(|| "the model turn failed".to_string()),
                ),
                _ => None,
            }
        }
    }
}

/// Derive the turn's outcome. An aborted turn stays `Aborted` even when the
/// run future errored on its way out (the abort is the closer); such an
/// error is `Lost` rather than a terminal failure, but it still reads as
/// `failed` exactly as `Result::is_err` did before.
pub(super) fn derive_outcome(
    result: &anyhow::Result<Vec<AgentMessage>>,
    abort_requested: bool,
) -> RunOutcome {
    if abort_requested {
        return match result {
            Err(err) => RunOutcome::Failed {
                terminal: Some(format!("{err:#}")),
                cause: FailureCause::Lost,
            },
            Ok(_) => RunOutcome::Aborted,
        };
    }
    match terminal_error(result) {
        Some(terminal) => RunOutcome::Failed {
            terminal: Some(terminal),
            cause: FailureCause::Terminal,
        },
        None => RunOutcome::Completed,
    }
}

/// The durable `turn_start` payload: the turn's open row stamps the owning
/// process (§C.2 turnStart owner) — the journal is shared across processes, and
/// a reader of an open turn needs the owner's liveness to settle a dead turn on
/// its behalf without ever killing one another process is running.
pub(super) fn turn_start_payload() -> serde_json::Value {
    serde_json::json!({ "owner": manox_harness::session::TurnOwner::for_current_process() })
}

/// Whether any open ask card was delivered by a run of THIS process (a
/// stale card re-parked at restore never holds a turn: its turn predates
/// the process, so no settle of ours may close or seed it).
pub(super) fn live_ask_pending(state: &Arc<EngineState>) -> bool {
    let stale = state.stale_questions.lock().unwrap();
    state
        .question_gate
        .pending_entries()
        .iter()
        .any(|(id, _)| !stale.contains(id))
}

/// Journal one settled ask's decision row at the caller's append point
/// (the actor's idle path — journal order ahead of the boundary the settle
/// implies). K4 fail-loud on permanent append failure: a durable loss
/// record plus one facade notice — a decision row that vanished silently
/// would keep a replayed card alive that no client can settle.
pub(super) async fn journal_ask_decision(
    appender: &Arc<JournalAppender>,
    id: &str,
    meta: &crate::permission::PendingAuthMeta,
    verdict: &str,
    state: &Arc<EngineState>,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
) {
    let payload = serde_json::json!({
        "kind": "decision",
        "authId": id,
        "payload": { "toolName": meta.tool_name, "verdict": verdict },
    });
    if let Err(err) = append_typed_resilient(appender, "question", payload).await {
        if let Some(row) = record_journal_loss(appender, "question", &err).await {
            state.pending_journal.lock().unwrap().push(row);
        }
        send_notice(
            notice_tx,
            BackendNotice::Event(Box::new(ThreadEvent::Error(anyhow::anyhow!(
                "journal append permanently failed for `question`: {err:#}; the entry was dropped"
            )))),
            "error notice",
        );
    }
}

/// Close a held turn: the answers settled (or the user dismissed / aborted /
/// submitted anew), so the parked close lands now through the same durable
/// tail every settle uses. `cancelled` is the close verdict: a normal answer
/// settles complete; an abort while answers were pending settles cancelled.
pub(super) async fn finish_held_settle(
    held: HeldTurn,
    cancelled: bool,
    appender: &Arc<JournalAppender>,
    state: &Arc<EngineState>,
    cwd: &Path,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
) {
    state.awaiting_answers.store(false, Ordering::Relaxed);
    state.awaiting_abort.store(false, Ordering::Relaxed);
    state.running.store(false, Ordering::Relaxed);
    let outcome = if cancelled {
        RunOutcome::Aborted
    } else {
        held.outcome
    };
    land_turn_finish(
        appender,
        state,
        notice_tx,
        cwd,
        cancelled || outcome.cancelled(),
        outcome.failed(),
        None,
        &held.steered,
        &[],
    )
    .await;
}

/// Post-settle goal housekeeping shared by every run: disarm automatic
/// continuation on any cancellation or run error (the goal keeps
/// its durable phase until a human resume re-arms it), and admit the goal
/// round that just ran when one was in flight.
pub(super) async fn goal_housekeeping(
    result: &anyhow::Result<Vec<AgentMessage>>,
    abort_requested: bool,
    session: &AgentSession,
    state: &Arc<EngineState>,
) {
    if (abort_requested || result.is_err())
        && let Some(bridge) = &state.goal_bridge
    {
        bridge.disarm();
    }
    if let Some(bridge) = &state.goal_bridge
        && bridge.goal_round_active()
    {
        let _ = crate::goal_driver::settle_goal_round(
            session,
            bridge,
            &state.goal_continuation_reserved,
            &state.goal_continuation_round,
            result,
            abort_requested,
        )
        .await;
    }
}

/// Chain automatic goal rounds from an idle position: gate first, run one
/// round, settle + housekeeping, repeat until no round is owed or the user
/// interrupts. The gate is consulted before every run, so a round is never
/// started on empty queues.
#[allow(clippy::too_many_arguments)] // actor plumbing: each input is distinct session state
pub(super) async fn chain_goal_rounds(
    session: &mut AgentSession,
    handle: &manox_harness::harness::HarnessHandle,
    cmd_rx: &mut mpsc::UnboundedReceiver<SessionCmd>,
    run_steers: &mut Vec<String>,
    shutdown_after_run: &mut bool,
    live: Arc<Mutex<LiveTranscript>>,
    state: &Arc<EngineState>,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
    harness_model: &mut HarnessModel,
    sessions_dir: &Path,
    cwd: &Path,
) {
    // A degraded session starts no run: the harness may be mid-state, and
    // every submit is refused until a restart.
    if state.is_degraded() {
        return;
    }
    // A held turn (open ask awaiting answers) never chains: the next run
    // would stack onto a turn that is still open on the wire.
    if state.awaiting_answers.load(Ordering::Relaxed) {
        return;
    }
    loop {
        let queued = match &state.goal_bridge {
            Some(bridge) => {
                crate::goal_driver::maybe_queue_goal_round(
                    session,
                    bridge,
                    &state.goal_continuation_reserved,
                    &state.goal_continuation_round,
                    handle,
                )
                .await
            }
            None => return,
        };
        if !queued {
            return;
        }
        let settlement = run_turn(
            session,
            TurnInput::Continue,
            handle,
            cmd_rx,
            run_steers,
            shutdown_after_run,
            Arc::clone(&live),
            state,
            notice_tx,
            harness_model,
            sessions_dir,
            cwd,
        )
        .await;
        if *shutdown_after_run {
            return;
        }
        goal_housekeeping(
            &settlement.result,
            settlement.abort_requested,
            session,
            state,
        )
        .await;
        // A round that ended on a delivered ask holds its turn — chaining
        // another round would stack onto the still-open turn.
        if settlement.held {
            return;
        }
        if settlement.abort_requested || settlement.outcome.failed() || state.is_degraded() {
            return;
        }
    }
}

/// S1/S2 shared continuation: run the kernel steering queue to empty from an
/// idle position, one `continue_` round at a time.
///
/// `continue_` drains the surviving steering queue first (its first poll
/// injects a message steered while idle and lands its durable `user` row —
/// pinned by the harness's `steering_queued_before_run_is_injected_by_loop`),
/// then settles + goal-chains exactly like a normal run. The loop is bounded
/// by `steering_messages()` becoming empty (each round consumes at least one)
/// and by shutdown/abort, so it can never spin: a queue that drains to one
/// message per `OneAtATime` round exits after that round's run.
///
/// This is the single entry point the actor uses for BOTH the wake-driven
/// resume (a producer steered events while idle) and the
/// between-runs facade `Steer` (S1: an idle steer now starts its own run
/// instead of waiting for the next turn), and it is called after every
/// normal settle to drain a steer that landed in the run's final moments
/// (S2).
///
/// Abort/failed discipline (S2 settle-drain guard — never pour a steer into
/// a dying or later run): a round
/// that aborted or errored stops the chain immediately — the commit already
/// retracted the not-yet-drained steers from the queue (S4), so a stranded
/// steer is never silently injected by a later run.
#[allow(clippy::too_many_arguments)] // actor plumbing: each input is distinct session state
pub(super) async fn resume_steering_queue(
    session: &mut AgentSession,
    cmd_rx: &mut mpsc::UnboundedReceiver<SessionCmd>,
    run_steers: &mut Vec<String>,
    shutdown_after_run: &mut bool,
    live: Arc<Mutex<LiveTranscript>>,
    state: &Arc<EngineState>,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
    harness_model: &mut HarnessModel,
    sessions_dir: &Path,
    cwd: &Path,
) {
    // Neither a degraded session nor a held turn resumes into a run: the
    // queued steer drains in the seeded (or next) run.
    if state.is_degraded() {
        return;
    }
    if state.awaiting_answers.load(Ordering::Relaxed) {
        return;
    }
    while !session.steering_messages().is_empty() {
        let handle = session.handle();
        let settlement = run_turn(
            session,
            TurnInput::Continue,
            &handle,
            cmd_rx,
            run_steers,
            shutdown_after_run,
            Arc::clone(&live),
            state,
            notice_tx,
            harness_model,
            sessions_dir,
            cwd,
        )
        .await;
        if *shutdown_after_run {
            return;
        }
        goal_housekeeping(
            &settlement.result,
            settlement.abort_requested,
            session,
            state,
        )
        .await;
        if *shutdown_after_run {
            return;
        }
        // Abort or run failure: stop chaining. The commit already retracted
        // the not-yet-drained steers (S4) and reported them stranded; an
        // aborted run must not auto-resume. A degraded session starts nothing.
        if settlement.abort_requested || settlement.outcome.failed() || state.is_degraded() {
            return;
        }
        // A round that ended on a delivered ask holds its turn — resuming
        // another round would stack onto the still-open turn.
        if settlement.held {
            return;
        }
        chain_goal_rounds(
            session,
            &handle,
            cmd_rx,
            run_steers,
            shutdown_after_run,
            Arc::clone(&live),
            state,
            notice_tx,
            harness_model,
            sessions_dir,
            cwd,
        )
        .await;
        if *shutdown_after_run {
            return;
        }
    }
}

/// Forward every run event through the adapt mapping onto the notice
/// channel as UI events.
pub(super) fn subscribe_session(
    session: &AgentSession,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
    live: Arc<Mutex<LiveTranscript>>,
    title: TitleScheduler,
) -> manox_harness::agent::Subscription {
    let event_tx = notice_tx.clone();
    // Seed the live mirror with the completed transcript so a mid-run tick
    // never drops restored history; the listener below appends from here.
    live.lock().unwrap().messages = session.harness_messages().to_vec();
    // A fresh session's jsonl file is deferred until the first assistant
    // message, so the sidebar only learns the thread exists once that file
    // materializes. The user MessageEnd fires before it; the first assistant
    // MessageEnd (the materialization moment — the persistence middleware
    // appends before listeners observe) is the authoritative signal.
    let assistant_signal_sent = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let assistant_flag = std::sync::Arc::clone(&assistant_signal_sent);
    session.subscribe(Arc::new(move |event, _cancel| {
        let tx = event_tx.clone();
        let assistant_flag = std::sync::Arc::clone(&assistant_flag);
        let live = std::sync::Arc::clone(&live);
        let title = title.clone();
        Box::pin(async move {
            // Mirror the in-flight transcript for the live-history ticker:
            // completed messages accumulate, the streaming partial replaces
            // the slot until `MessageEnd` seals it.
            match &event {
                AgentEvent::MessageStart { message }
                | AgentEvent::MessageUpdate { message, .. } => {
                    live.lock().unwrap().streaming = Some((**message).clone());
                }
                AgentEvent::MessageEnd { message } => {
                    let mut guard = live.lock().unwrap();
                    guard.streaming = None;
                    guard.messages.push((**message).clone());
                }
                _ => {}
            }
            if let AgentEvent::MessageEnd { message } = &event {
                match &**message {
                    AgentMessage::User { .. } => {
                        send_notice(&tx, BackendNotice::SessionListDirty, "session list dirty");
                    }
                    AgentMessage::Assistant { .. }
                        if !assistant_flag.swap(true, std::sync::atomic::Ordering::SeqCst) =>
                    {
                        // First assistant message: the deferred session file
                        // just materialized, so the sidebar can list it.
                        send_notice(&tx, BackendNotice::SessionListDirty, "session list dirty");
                    }
                    _ => {}
                }
            }
            title.observe(&event);
            for te in adapt::agent_event_to_thread_events(&event) {
                send_notice(&tx, BackendNotice::Event(Box::new(te)), "engine notice");
            }
        })
    }))
}

/// Cheap change fingerprint for the live-history guard: message count plus
/// the trailing message's content size. Collisions only defer a facade
/// refresh by one tick, so exactness is unnecessary. Notes ride alongside
/// but never contribute: the `notes_gen` guard covers them.
pub(super) fn live_fingerprint(mapped: &[HistoryEntry]) -> (usize, usize) {
    let trailing = mapped
        .iter()
        .filter_map(|e| match e {
            HistoryEntry::Message(m) => Some(m),
            _ => None,
        })
        .next_back()
        .map(|m| {
            m.content
                .iter()
                .map(|c| match c {
                    MessageContent::Text(t) => t.len(),
                    MessageContent::Thinking { text, .. } => text.len(),
                    MessageContent::Image { data, .. } => data.len(),
                    MessageContent::Compaction(t) => t.len(),
                    MessageContent::ToolUse(t) => t.name.len() + t.input.to_string().len(),
                    MessageContent::ToolResult(t) => t.content.len(),
                })
                .sum()
        })
        .unwrap_or(0);
    let count = mapped
        .iter()
        .filter(|e| matches!(e, HistoryEntry::Message(_)))
        .count();
    (count, trailing)
}

/// Refresh the engine's history mirror from the live transcript snapshot
/// (completed messages + the streaming partial). Returns whether the mirror
/// changed, so the caller can skip the facade notice on idle ticks (e.g. a
/// run parked on a user interaction where nothing is streaming).
pub(super) fn sync_live_history(
    live: &Arc<Mutex<LiveTranscript>>,
    state: &Arc<EngineState>,
) -> bool {
    let mut msgs: Vec<AgentMessage> = Vec::new();
    {
        let guard = live.lock().unwrap();
        msgs.extend(guard.messages.iter().cloned());
        if let Some(streaming) = &guard.streaming {
            msgs.push(streaming.clone());
        }
    }
    let mut display: Vec<HistoryEntry> = adapt::harness_messages_to_messages(&msgs)
        .into_iter()
        .map(HistoryEntry::Message)
        .collect();
    let notes_gen = state.notes_gen.load(Ordering::SeqCst);
    let mut history = state.history.lock().unwrap();
    if live_fingerprint(&history) == live_fingerprint(&display)
        && notes_gen == state.notes_gen.load(Ordering::SeqCst)
    {
        return false;
    }
    // The live transcript carries no custom entries: re-merge the positioned
    // notes so a mid-run switch-back keeps the cards at their position.
    let notes = state.notes.lock().unwrap().clone();
    merge_positioned_notes(&mut display, &notes);
    *history = display;
    true
}

/// Interleave notes right after their `after_message`-th message; a count of
/// zero lands at the top, an over-long count clamps to the tail. Notes are
/// stored in append order (non-decreasing positions), so sequential inserts
/// preserve their relative order.
pub(super) fn merge_positioned_notes(display: &mut Vec<HistoryEntry>, notes: &[PositionedNote]) {
    for positioned in notes {
        let target = positioned.after_message;
        let mut insert_at = if target == 0 { 0 } else { display.len() };
        let mut count = 0usize;
        for (i, entry) in display.iter().enumerate() {
            if matches!(entry, HistoryEntry::Message(_)) {
                count += 1;
                if count == target {
                    insert_at = i + 1;
                }
            }
        }
        display.insert(insert_at, HistoryEntry::Note(positioned.note.clone()));
    }
}

/// Adapt harness lifecycle events onto the notice channel. Carries the
/// compaction visibility pair:
/// start flips the UI into its summarizing state, a successful end lands the
/// Recap card. The end event's token counts ride the result; the UI chrome
/// consumes only the summary.
pub(super) fn subscribe_harness_events(
    session: &mut AgentSession,
    sessions_dir: PathBuf,
    session_path: PathBuf,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
    wakeup_tx: &mpsc::UnboundedSender<()>,
) -> manox_harness::harness::HarnessSubscription {
    let tx = notice_tx.clone();
    let wake = wakeup_tx.clone();
    session.subscribe_harness(Arc::new(move |event| match event {
        // Idle-wakeup signal: a producer steered events into the queue (a
        // background task completion, a subagent finish). The
        // actor decides whether the session is idle and resumes it
        // (`continue_` drains the steering queue first) — the listener stays
        // stateless and never touches the session itself.
        manox_harness::harness::HarnessEvent::QueueUpdate { steer, .. } if steer > 0 => {
            let _ = wake.send(());
        }
        manox_harness::harness::HarnessEvent::CompactionStart { .. } => {
            send_notice(
                &tx,
                BackendNotice::Event(Box::new(ThreadEvent::CompactionStarted {
                    tokens_before: 0,
                })),
                "compaction started notice",
            );
        }
        manox_harness::harness::HarnessEvent::CompactionEnd {
            result: Some(result),
            aborted: false,
            ..
        } => {
            // The transcript was rebuilt as a summary user message that
            // consumes a display ordinal, so the sidecar's registry display
            // forms no longer align; drop them so a reload never mislabels a
            // prompt. Fire-and-forget: the manual `/compact` path awaits the
            // same clear before mirroring.
            clear_user_chrome_spawn(sessions_dir.clone(), session_path.clone());
            let retained_tail = adapt::harness_messages_to_messages(&result.retained_tail);
            send_notice(
                &tx,
                BackendNotice::Event(Box::new(ThreadEvent::Compaction {
                    summary: result.summary,
                    messages_compacted: 0,
                    tokens_before: result.tokens_before,
                    retained_tail,
                })),
                "compaction notice",
            );
        }
        _ => {}
    }))
}

/// Session header metadata: the creating host's identity, the owning thread
/// id (copied verbatim by the worktree fork, so every fork file groups under
/// one thread), plus (for a team worker) the
/// leader's session id. The links persist with the jsonl
/// file, so they survive restarts and outlive the in-memory team.
pub(super) fn session_metadata(thread_id: &str, parent_session: Option<&str>) -> serde_json::Value {
    let mut metadata = serde_json::json!({
        "host": crate::host::current().slug(),
        "thread": thread_id,
    });
    if let Some(parent) = parent_session {
        metadata["team"] = serde_json::json!({ "parent": parent });
    }
    metadata
}
