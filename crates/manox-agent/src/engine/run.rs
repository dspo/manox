//! Engine run plumbing: driving a run to settlement, goal chaining,
//! steering resume, live-history sync, and harness subscriptions.

use super::*;

/// Drive one session run to completion while still servicing mid-run
/// commands (abort/steer/cancel/shutdown) through the session handle.
/// Shared by user prompts, monitor idle-wakeups, and plan-approval seeds.
/// Returns the run result and whether an abort was requested.
///
/// While the run is in flight, a periodic tick refreshes the engine's
/// history mirror from the live transcript (`LiveHistory` notice) so a
/// thread switched back to mid-turn rebuilds from current progress.
#[allow(clippy::too_many_arguments)] // drive plumbing: each input is a distinct sink
pub(super) async fn drive_run<F>(
    run: F,
    handle: &manox_harness::harness::HarnessHandle,
    cmd_rx: &mut mpsc::UnboundedReceiver<SessionCmd>,
    run_steers: &mut Vec<String>,
    shutdown_after_run: &mut bool,
    live: Arc<Mutex<LiveTranscript>>,
    state: &Arc<EngineState>,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
    harness_model: &mut HarnessModel,
    sessions_dir: &Path,
    session_path: &Path,
    appender: &Arc<JournalAppender>,
) -> (anyhow::Result<Vec<AgentMessage>>, bool)
where
    F: std::future::Future<Output = anyhow::Result<Vec<AgentMessage>>>,
{
    tokio::pin!(run);
    // W4 boundary: a pending plan-mode selection commits at the start of
    // whatever run comes next — a user prompt, a drained steer, a goal round
    // or a monitor wake-up — so no run can execute under the old mode while
    // the projection already shows the selection pending. It lands before the
    // run future is polled, hence before request assembly or the injected
    // instructions can observe the old state.
    commit_requested_plan_mode(session_path, sessions_dir, state, notice_tx).await;
    // Live journal appends run on a dedicated serializer task, never inline
    // in this select. The `run` branch below shares THIS task, and its
    // persistence middleware holds the session's append lock across file-I/O
    // await points (`append_message_with_origin` → `append_line`); a select
    // handler that awaits the same lock suspends the whole task, so the
    // select can never poll the run branch again to release it — a same-task
    // self-deadlock that froze the turn forever with the journal tail stuck
    // mid-round (the round-11 stall). The serializer keeps facade rows
    // ordered (FIFO channel, sequential appends) and stays linearized
    // against middleware appends by the same session append lock.
    let (live_row_tx, mut live_row_rx) = mpsc::unbounded_channel::<(String, serde_json::Value)>();
    let live_appender = Arc::clone(appender);
    let live_state = Arc::clone(state);
    let live_notice = notice_tx.clone();
    let live_handle = handle.clone();
    // K4 fail-closed flag: a permanent mid-run append loss cancels the turn;
    // drive_run folds this into `abort_requested` so settle reports it.
    let live_abort = Arc::new(AtomicBool::new(false));
    let live_abort_flag = Arc::clone(&live_abort);
    let live_appends = tokio::spawn(async move {
        let mut loss_signaled = false;
        while let Some((kind, payload)) = live_row_rx.recv().await {
            let Err(err) = append_typed_resilient(&live_appender, &kind, payload.clone()).await
            else {
                continue;
            };
            // Permanent loss (K4, L3): the state this row describes already
            // took effect, so the run must not silently continue without it.
            // The first loss records itself durably, notifies the facade,
            // and cancels the turn; later rows of the same dead-storage run
            // park for the settle/idle drain without re-signaling (one
            // notice, one abort — the turn is already converging).
            if loss_signaled {
                live_state
                    .pending_journal
                    .lock()
                    .unwrap()
                    .push((kind, payload));
                continue;
            }
            if let Some(row) = record_journal_loss(&live_appender, &kind, &err).await {
                live_state.pending_journal.lock().unwrap().push(row);
            }
            if kind != "error" {
                loss_signaled = true;
                send_notice(
                    &live_notice,
                    BackendNotice::Event(Box::new(ThreadEvent::Error(anyhow::anyhow!(
                        "journal append permanently failed for `{kind}`: {err:#}; cancelling the turn"
                    )))),
                    "journal-loss turn cancellation",
                );
                live_abort_flag.store(true, Ordering::SeqCst);
                live_handle.abort();
            }
        }
    });
    let mut abort_requested = false;
    let mut channel_open = true;
    let mut live_ticker = tokio::time::interval(LIVE_HISTORY_TICK);
    live_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first tick completes immediately; consume it so the mirror is not
    // re-synced at run start (the settle/ready path already mirrored).
    live_ticker.tick().await;
    // Stall watchdog: a turn whose journal tail stops moving while the run
    // future stays pending is the round-11 stall shape (tools finished, no
    // durable rows, no settle). The warn carries the frozen cursor so the
    // log bracket around the stall is unmissable; it repeats every 2 minutes.
    let mut watchdog_cursor: Option<u64> = None;
    let mut watchdog_last_move = std::time::Instant::now();
    let appender_for_watchdog = appender;
    let result = loop {
        if !channel_open {
            break run.await;
        }
        tokio::select! {
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
                }
                Some(SessionCmd::Steer { id, text, images }) => {
                    // S4: enqueue under the command id (== the client
                    // message id post-S3) so a later `cancel_steer(&id)` can
                    // retract this exact steer from the kernel queue if the
                    // run aborts before draining it. S3: the same id rides the
                    // injected `user` row as its durable identity.
                    handle.steer_with_id(steer_message(id.clone(), text, images), id.clone());
                    run_steers.push(id);
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
                    // play is never re-queued.
                    if *harness_model != new_model && handle.set_model(new_model.clone()) {
                        *state.model.lock().unwrap() = Some(new_model.clone());
                        *harness_model = new_model;
                    }
                }
                Some(SessionCmd::SetThinkingLevel(level)) => {
                    // Mid-run switch: the queued mutation lands at the next
                    // turn boundary (same semantics as `set_model`); persist
                    // the choice so a reopened session restores it.
                    handle.set_thinking_level(level.clone());
                    if let Some(effort) = level.as_deref().and_then(parse_reasoning_effort)
                        && let Err(err) = write_reasoning_effort_sidecar(
                            sessions_dir,
                            session_path,
                            effort,
                        )
                        .await
                    {
                        tracing::warn!(error = %err, "failed to persist reasoning effort");
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
                    // the idle loop (the run owns the `Session`; a second
                    // writer could fork the leaf cursor).
                    mirror_ui_note(state, record.clone());
                    state.pending_ui_notes.lock().unwrap().push(record);
                }
                Some(SessionCmd::AppendJournal { kind, payload }) => {
                    // Forwarded to the serializer task spawned above — this
                    // arm must NEVER await the session's append lock inline:
                    // the run branch of this same select suspends holding it
                    // (the persistence middleware's file I/O), and a handler
                    // await would deadlock the task against itself (the
                    // round-11 stall). Rows still append LIVE through the
                    // shared session handle in emission order; parking for
                    // settle hid subagent/retry/background rows for the whole
                    // run (L5 — the round-8 repro: a dispatched Sailor's
                    // failure never reached the journal while the captain
                    // kept working).
                    let _ = live_row_tx.send((kind, payload));
                }
                Some(SessionCmd::JournalSnapshot { reply }) => {
                    // Answer live off the storage (same read face as the
                    // idle loop): parking froze GetConversationInfo and
                    // PageHistory — the Q face AND the follow streams' gap
                    // repair — for the entire duration of a running turn.
                    reply_journal_snapshot(appender, reply).await;
                }
                Some(SessionCmd::RequestPlanMode { enabled }) => {
                    // Mid-run selection: record the intent and journal it
                    // LIVE (the serializer path — never an inline session
                    // append, see the AppendJournal arm's deadlock note).
                    // The commit stays on the next turn boundary, which is
                    // what makes `plan_mode_pending` observable and the
                    // selection revocable before it takes effect. The request
                    // row is best-effort here: a permanent serializer failure
                    // parks a loss record and drops the row while the
                    // selection still commits at the boundary (the idle arm
                    // appends synchronously and therefore drops the selection
                    // instead of committing without its request).
                    state.plan.set_requested(Some(enabled));
                    let _ = live_row_tx.send((
                        "plan_mode_request".into(),
                        serde_json::json!({ "enabled": enabled }),
                    ));
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
                    }
                }
            },
            result = &mut run => break result,
        }
    };
    // Drain the serializer before returning: every AppendJournal received
    // mid-run has landed (or parked for settle) before settle_run reads the
    // journal. The run future has finished, so the append lock is free and
    // the drain is bounded by the queued rows.
    drop(live_row_tx);
    let _ = live_appends.await;
    // A K4 fail-closed cancel (permanent journal loss) counts as the abort
    // it was: settle reports `cancelled` and strands the run's steers.
    if live_abort.load(Ordering::SeqCst) {
        abort_requested = true;
    }
    (result, abort_requested)
}

/// Post-run settlement shared by user prompts and monitor idle-wakeups:
/// error notice, running flag, history/usage/session-list mirrors, steer
/// accounting, title eligibility, and the `Settled` notice.
#[allow(clippy::too_many_arguments)] // settlement plumbing: each input is a distinct sink
pub(super) async fn settle_run(
    result: &anyhow::Result<Vec<AgentMessage>>,
    abort_requested: bool,
    session: &AgentSession,
    state: &Arc<EngineState>,
    sessions_dir: &Path,
    cwd: &Path,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
    run_steers: &mut Vec<String>,
) {
    let failed = result.is_err();
    // K5: a pin no run consumed (one died before announcing its user
    // message) must not leak the middleware skip into the next turn.
    session.journal_appender().clear_accepted_user_entry();
    if let Err(err) = result {
        send_notice(
            notice_tx,
            BackendNotice::Event(Box::new(ThreadEvent::Error(anyhow::anyhow!("{err:#}")))),
            "error notice",
        );
    }
    state.running.store(false, Ordering::Relaxed);
    // The sticky cwd may have moved during the run (a tool call with an
    // explicit `cwd`, or a `cd` inside a command): report one `CwdChanged`
    // per durable move so the facade mirror and the UI track the session
    // tail. The projected cwd comes from the session path — the flush has
    // already made the move durable at this point.
    let projected = session.projected_cwd().await;
    let projected_str = projected.to_string_lossy().into_owned();
    let last = state.last_cwd_note.lock().unwrap().clone();
    if last.as_deref() != Some(projected_str.as_str()) {
        *state.last_cwd_note.lock().unwrap() = Some(projected_str.clone());
        send_notice(
            notice_tx,
            BackendNotice::Event(Box::new(ThreadEvent::CwdChanged {
                path: projected_str,
            })),
            "cwd changed notice",
        );
    }
    // Mid-run appends parked their persistence (the run owned the session);
    // persist before the authoritative sync so the rebuilt mirror keeps them.
    let parked = std::mem::take(&mut *state.pending_ui_notes.lock().unwrap());
    for record in parked {
        let _ = persist_ui_note(session, state, notice_tx, &record).await;
    }
    let parked_journal = std::mem::take(&mut *state.pending_journal.lock().unwrap());
    if !parked_journal.is_empty() {
        // K4: the settle drain runs the same fail-loud discipline as every
        // typed-append face — bounded retries, then a durable loss record
        // and one facade notice. The turn has already converged here, so
        // there is nothing left to cancel; the record parks for the idle
        // drain when the storage is still down (it lands after recovery).
        let appender = session.journal_appender();
        let mut loss_notified = false;
        for (kind, payload) in parked_journal {
            if let Err(err) = append_typed_resilient(&appender, &kind, payload).await {
                if let Some(row) = record_journal_loss(&appender, &kind, &err).await {
                    state.pending_journal.lock().unwrap().push(row);
                }
                if kind != "error" && !loss_notified {
                    loss_notified = true;
                    send_notice(
                        notice_tx,
                        BackendNotice::Event(Box::new(ThreadEvent::Error(anyhow::anyhow!(
                            "journal append permanently failed for `{kind}` at settle: {err:#}; the entry was dropped"
                        )))),
                        "error notice",
                    );
                }
            }
        }
    }
    sync_history(session, sessions_dir, state).await;
    sync_usage(session, state).await;
    spawn_session_list_refresh(sessions_dir, state);
    // S4: the kernel steering queue survives runs, so a steer that never
    // drained during an aborted/failed run would silently inject into the
    // NEXT run (a duplicated user message if the client retries). Retract
    // each queued steer by its command id; a steer whose cancel fails was
    // already drained by THIS run, so it is genuinely steered (its row is
    // on disk), not stranded. `abort_requested || failed` only retracts the
    // still-queued tail (a pending steer is discarded/failed,
    // never poured into a dying or later run).
    let (steered, stranded) = if abort_requested || failed {
        let mut steered = Vec::new();
        let mut stranded = Vec::new();
        for id in std::mem::take(run_steers) {
            if session.cancel_steer(&id) {
                stranded.push(id);
            } else {
                steered.push(id);
            }
        }
        (steered, stranded)
    } else {
        (std::mem::take(run_steers), Vec::new())
    };
    // §C.2 `turn_finish`: the settle verdict must ride the journal, not
    // just the notice plane — v2 clients fold lifecycle facts from durable
    // rows (the `ThreadEvent::TurnFinished` notice is `Skip`ped by
    // translate), and the app's steer-card retirement keys off this row.
    // Appended through the shared writer at the session's single append
    // point (id/parent/timestamp assigned under the append lock, so the
    // chain stays dense and the seq is the natural next one). The K4
    // fail-loud discipline mirrors the parked drain above: bounded retry,
    // durable loss record, one facade notice on permanent failure.
    let turn_finish = serde_json::json!({
        "cancelled": abort_requested,
        "failed": failed,
        "strandedSteerIds": stranded.clone(),
    });
    let appender = session.journal_appender();
    if let Err(err) = append_typed_resilient(&appender, "turn_finish", turn_finish).await {
        if let Some(row) = record_journal_loss(&appender, "turn_finish", &err).await {
            state.pending_journal.lock().unwrap().push(row);
        }
        send_notice(
            notice_tx,
            BackendNotice::Event(Box::new(ThreadEvent::Error(anyhow::anyhow!(
                "journal append permanently failed for `turn_finish` at settle: {err:#}; the entry was dropped"
            )))),
            "error notice",
        );
    }
    send_notice(
        notice_tx,
        BackendNotice::Settled {
            cancelled: abort_requested,
            failed,
            steered,
            stranded,
        },
        "run settled",
    );
    // Plugin lifecycle: `Stop` fires on every settled turn (fail-open,
    // detached) — after the `Settled` notice so observers see the turn's
    // final state first.
    crate::plugin_hooks::fire(
        crate::plugin_hooks::HookEvent::Stop,
        cwd.to_str(),
        serde_json::json!({
            "cancelled": abort_requested,
            "failed": failed,
        }),
    );
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
    session_path: &Path,
) {
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
        let journal_appender = session.journal_appender();
        let (result, abort_requested) = drive_run(
            session.continue_(),
            handle,
            cmd_rx,
            run_steers,
            shutdown_after_run,
            Arc::clone(&live),
            state,
            notice_tx,
            harness_model,
            sessions_dir,
            session_path,
            &journal_appender,
        )
        .await;
        settle_run(
            &result,
            abort_requested,
            session,
            state,
            sessions_dir,
            cwd,
            notice_tx,
            run_steers,
        )
        .await;
        if *shutdown_after_run {
            return;
        }
        goal_housekeeping(&result, abort_requested, session, state).await;
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
/// resume (a monitor steered events while idle) and the
/// between-runs facade `Steer` (S1: an idle steer now starts its own run
/// instead of waiting for the next turn), and it is called after every
/// normal settle to drain a steer that landed in the run's final moments
/// (S2).
///
/// Abort/failed discipline (S2 settle-drain guard — never pour a steer into
/// a dying or later run): a round
/// that aborted or errored stops the chain immediately — `settle_run` already
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
    while !session.steering_messages().is_empty() {
        let handle = session.handle();
        let active_session_path = session.path().clone();
        let journal_appender = session.journal_appender();
        // The facade learns the run started (`TurnStarted` sets its running
        // flag) so a switch-away parks the thread instead of dropping it
        // mid-run — identical to the wake branch this helper now subsumes.
        state.running.store(true, Ordering::Relaxed);
        send_notice(
            notice_tx,
            BackendNotice::Event(Box::new(ThreadEvent::TurnStarted)),
            "turn started notice",
        );
        let (result, abort_requested) = drive_run(
            session.continue_(),
            &handle,
            cmd_rx,
            run_steers,
            shutdown_after_run,
            Arc::clone(&live),
            state,
            notice_tx,
            harness_model,
            sessions_dir,
            &active_session_path,
            &journal_appender,
        )
        .await;
        settle_run(
            &result,
            abort_requested,
            session,
            state,
            sessions_dir,
            cwd,
            notice_tx,
            run_steers,
        )
        .await;
        if *shutdown_after_run {
            return;
        }
        goal_housekeeping(&result, abort_requested, session, state).await;
        if *shutdown_after_run {
            return;
        }
        // Abort or run error: stop chaining. `settle_run` already retracted the
        // not-yet-drained steers (S4) and reported them stranded; an aborted
        // run must not auto-resume.
        if abort_requested || result.is_err() {
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
            &active_session_path,
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
        // Idle-wakeup signal: a monitor steered events into the queue. The
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
/// id (the retired worktree fork copied it verbatim, so historical fork
/// files still group under one thread), plus (for a team worker) the
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
