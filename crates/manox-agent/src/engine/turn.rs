//! Turn ownership: open -> drive -> commit -> project.
//!
//! A turn closes exactly once. The commit is in memory first and durable
//! second — one `turn_finish` row plus its side effects (ask withdrawals, the
//! `Settled` notice, the Stop hook) — and it runs BEFORE any derived work.
//! Everything derived (cwd notice, parked persistence, the history mirror,
//! usage, the session list) lives in the projection: detached, single-flight,
//! monotone by journal seq. A projection that never finishes leaves the
//! mirrors stale, never the turn open.

use futures::FutureExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use manox_harness::coding_agent::AgentSession;
use manox_harness::harness::HarnessHandle;
use manox_harness::session::Event;
use manox_harness::types::{AgentMessage, ContentBlock};
use tokio::sync::mpsc;

use super::*;

/// How long an aborted run future gets to resolve on its own. A future that
/// outlives the grace is dropped: the harness may be mid-state, so the session
/// degrades instead of pretending the turn closed cleanly.
pub(super) const ABORT_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

/// The window a projection rerun takes: a request that lands while one is
/// running only raises the dirty flag (single-flight, rerun once).
#[derive(Default)]
pub(super) struct ProjectionFlight {
    pub(super) running: bool,
    pub(super) dirty: bool,
}

/// What a committed turn reported, mirroring `BackendNotice::Settled`.
#[derive(Debug, Clone, Default)]
pub(super) struct CommitFacts {
    pub(super) cancelled: bool,
    pub(super) failed: bool,
    pub(super) steered: Vec<String>,
    pub(super) stranded: Vec<String>,
    /// The turn closed on a delivered ask: its close is parked in
    /// `EngineState::held_settle` until the answer settles.
    pub(super) held: bool,
}

/// The turn's in-flight ownership, constructed when the turn opens.
///
/// It holds everything the durable close needs — the journal handle, the
/// kernel control handle, the actor's state and sinks, the cwd and the steer
/// ids delivered so far — so the close never borrows `AgentSession` and never
/// depends on the run future. `commit_turn` consumes it; `Drop` closes the turn
/// durably from the runtime if it is dropped between open and commit (a panic
/// or an unwound actor in that window), which is what makes "a turn always
/// closes" structural rather than a watchdog.
pub(super) struct TurnCommit {
    appender: Arc<JournalAppender>,
    handle: HarnessHandle,
    state: Arc<EngineState>,
    notice_tx: mpsc::UnboundedSender<BackendNotice>,
    cwd: PathBuf,
    steers: Vec<String>,
    committed: bool,
}

impl TurnCommit {
    pub(super) fn open(
        session: &AgentSession,
        state: &Arc<EngineState>,
        notice_tx: &mpsc::UnboundedSender<BackendNotice>,
        cwd: &Path,
    ) -> Self {
        Self {
            appender: session.journal_appender(),
            handle: session.handle(),
            state: Arc::clone(state),
            notice_tx: notice_tx.clone(),
            cwd: cwd.to_path_buf(),
            steers: Vec::new(),
            committed: false,
        }
    }

    /// The journal handle this turn commits through.
    pub(super) fn appender(&self) -> &Arc<JournalAppender> {
        &self.appender
    }

    /// Adopt the steer ids delivered before the turn opened (an idle steer
    /// that started this turn) and, later, every mid-run steer.
    pub(super) fn adopt_steers(&mut self, steers: Vec<String>) {
        self.steers.extend(steers);
    }

    pub(super) fn record_steer(&mut self, id: String) {
        self.steers.push(id);
    }

    /// Take the delivered steer ids (a caller that closes the turn through a
    /// fresh commit hands them over).
    #[cfg(test)]
    pub(super) fn take_steers(&mut self) -> Vec<String> {
        std::mem::take(&mut self.steers)
    }

    /// Close the turn: withdraw its ask cards, settle the steer accounting,
    /// land the durable `turn_finish`, then publish `Settled` and fire the
    /// Stop hook. Everything here is bounded and actor-side; derived work runs
    /// afterwards in the projection.
    pub(super) async fn commit_turn(
        mut self,
        outcome: RunOutcome,
        abort_requested: bool,
    ) -> CommitFacts {
        // Marked before the first await: if this future is dropped mid-commit
        // the row has either landed or been reported lost — never appended a
        // second time by `Drop`.
        self.committed = true;
        let failed = outcome.failed();
        let cancelled = outcome.cancelled() || abort_requested;
        // K5: a pin no run consumed (one died before announcing its user
        // message) must not leak the middleware skip into the next turn.
        self.appender.clear_accepted_user_entry();
        // The hold: a clean run that still has an open ask keeps its turn (and
        // the actor's running flag) until the answer settles. A dying run's
        // open cards cancel here — no turn will ever hold for them, and a card
        // whose turn is gone is unanswerable by construction. Only cards this
        // process delivered hold; restore-reparked cards are journal-only rows
        // (their turn predates the process).
        let hold = !abort_requested && !failed && live_ask_pending(&self.state);
        self.state.running.store(hold, Ordering::Relaxed);
        if !hold {
            withdraw_open_asks(&self.appender, &self.state, &self.notice_tx, "cancelled").await;
        }
        // S4: the kernel steering queue survives runs, so a steer that never
        // drained during an aborted/failed run would silently inject into the
        // NEXT run (a duplicated user message if the client retries). Retract
        // each queued steer by its command id; a steer whose cancel fails was
        // already drained by THIS run, so it is genuinely steered (its row is
        // on disk), not stranded.
        let (steered, stranded) = if abort_requested || failed {
            retract_steers(&self.handle, std::mem::take(&mut self.steers))
        } else {
            (std::mem::take(&mut self.steers), Vec::new())
        };
        if hold {
            // Park the close: the turn stays open until a settling answer (or
            // a new submit / dismiss / abort) closes it through
            // `finish_held_settle`, which runs this same durable tail.
            *self.state.held_settle.lock().unwrap() = Some(HeldTurn {
                outcome,
                steered: steered.clone(),
            });
            self.state.awaiting_answers.store(true, Ordering::Relaxed);
            return CommitFacts {
                cancelled: false,
                failed: false,
                steered,
                stranded,
                held: true,
            };
        }
        land_turn_finish(
            &self.appender,
            &self.state,
            &self.notice_tx,
            &self.cwd,
            cancelled,
            failed,
            None,
            &steered,
            &stranded,
        )
        .await;
        CommitFacts {
            cancelled,
            failed,
            steered,
            stranded,
            held: false,
        }
    }
}

impl Drop for TurnCommit {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        self.committed = true;
        // The turn was abandoned between open and commit (a panic in the drive
        // loop). Close it durably anyway, detached: the actor task may itself
        // be unwinding, so nothing here may await.
        let Some(runtime) = crate::runtime::try_handle() else {
            tracing::error!(
                "turn dropped before commit and no runtime is available; its close cannot land"
            );
            return;
        };
        let appender = self.appender.clone();
        let state = self.state.clone();
        let notice_tx = self.notice_tx.clone();
        let cwd = self.cwd.clone();
        let handle = self.handle.clone();
        let steers = std::mem::take(&mut self.steers);
        state.mark_degraded("the turn was dropped between open and commit");
        runtime.spawn(async move {
            withdraw_open_asks(&appender, &state, &notice_tx, "cancelled").await;
            let (steered, stranded) = retract_steers(&handle, steers);
            land_turn_finish(
                &appender,
                &state,
                &notice_tx,
                &cwd,
                false,
                true,
                Some("the turn was dropped before its close"),
                &steered,
                &stranded,
            )
            .await;
        });
    }
}

/// A turn whose clean close is parked on a delivered ask.
pub(super) struct HeldTurn {
    /// The outcome the run reached (always `Completed` by construction: only
    /// a clean run holds).
    pub(super) outcome: RunOutcome,
    pub(super) steered: Vec<String>,
}

/// A run's durable close. Shared by [`TurnCommit::commit`], the held close and
/// the restore reconciliation, so every `turn_finish` row in a session has one
/// writer shape.
#[allow(clippy::too_many_arguments)] // the close names each field the row carries
pub(super) async fn land_turn_finish(
    appender: &Arc<JournalAppender>,
    state: &Arc<EngineState>,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
    cwd: &Path,
    cancelled: bool,
    failed: bool,
    reason: Option<&str>,
    steered: &[String],
    stranded: &[String],
) {
    let mut turn_finish = serde_json::json!({
        "cancelled": cancelled,
        "failed": failed,
        "strandedSteerIds": stranded,
    });
    if let Some(reason) = reason {
        turn_finish["reason"] = serde_json::json!(reason);
    }
    // K4 fail-loud: bounded retry, a durable loss record, one facade notice.
    // The row is the durable close every replay folds — losing it silently
    // would leave a turn that never ends.
    if let Err(err) = append_typed_resilient(appender, "turn_finish", turn_finish).await {
        if let Some(row) = record_journal_loss(appender, "turn_finish", &err).await {
            lock_mirror(&state.pending_journal).push(row);
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
            cancelled,
            failed,
            steered: steered.to_vec(),
            stranded: stranded.to_vec(),
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
            "cancelled": cancelled,
            "failed": failed,
        }),
    );
}

/// Withdraw every delivered ask card of this process's gate, journaling one
/// decision row per card. Stale (restore-reparked) cards are withdrawn too:
/// the verdict is journal-only for them, but a card whose turn is gone must
/// not survive in the gate either.
pub(super) async fn withdraw_open_asks(
    appender: &Arc<JournalAppender>,
    state: &Arc<EngineState>,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
    verdict: &str,
) {
    if state.question_gate.pending_entries().is_empty() {
        return;
    }
    for (id, meta) in state.question_gate.withdraw_all() {
        journal_ask_decision(appender, &id, &meta, verdict, state, notice_tx).await;
    }
}

/// Retract the still-queued steers of a dying turn. Returns
/// `(steered, stranded)`: a cancel that fails retracted nothing, so the run had
/// already drained that steer (its row is on disk).
pub(super) fn retract_steers(
    handle: &HarnessHandle,
    ids: Vec<String>,
) -> (Vec<String>, Vec<String>) {
    let mut steered = Vec::new();
    let mut stranded = Vec::new();
    for id in ids {
        if handle.cancel_steer(&id) {
            stranded.push(id);
        } else {
            steered.push(id);
        }
    }
    (steered, stranded)
}

// ── the one turn entry point ─────────────────────────────────────────────────

/// What opens a turn. Every run site (user prompt, goal round, steering
/// resume) goes through [`run_turn`], so the open -> drive -> commit ->
/// project order has exactly one implementation.
pub(super) enum TurnInput {
    /// A user or peer prompt; its user row is already on the chain
    /// (`persist_prompt_user_entry` ran before the turn opened).
    Prompt {
        text: String,
        images: Vec<ContentBlock>,
    },
    /// Continue the transcript (goal rounds, steering resume).
    Continue,
}

/// What one turn left behind for its callers: the run result for goal
/// housekeeping, the derived outcome for the chaining decisions, and whether
/// the close was parked on an ask.
pub(super) struct TurnSettlement {
    pub(super) result: anyhow::Result<Vec<AgentMessage>>,
    pub(super) outcome: RunOutcome,
    pub(super) abort_requested: bool,
    pub(super) held: bool,
}

#[allow(clippy::too_many_arguments)] // actor plumbing: each input is distinct session state
pub(super) async fn run_turn(
    session: &mut AgentSession,
    input: TurnInput,
    handle: &HarnessHandle,
    cmd_rx: &mut mpsc::UnboundedReceiver<SessionCmd>,
    run_steers: &mut Vec<String>,
    shutdown_after_run: &mut bool,
    live: Arc<Mutex<LiveTranscript>>,
    state: &Arc<EngineState>,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
    harness_model: &mut HarnessModel,
    sessions_dir: &Path,
    cwd: &Path,
) -> TurnSettlement {
    let session_path = session.path().clone();
    // ── open ────────────────────────────────────────────────────────────
    let mut turn = TurnCommit::open(session, state, notice_tx, cwd);
    turn.adopt_steers(std::mem::take(run_steers));
    state.running.store(true, Ordering::Relaxed);
    // The turn's open edge, landed BEFORE the run: the durable `turn_start`
    // row goes through the shared writer synchronously (the notice tap is an
    // async hop, and a row that landed after the turn's own close would fold
    // as a second, phantom turn) and the notice then tells the facade.
    land_turn_start(turn.appender(), state, notice_tx).await;
    send_notice(
        notice_tx,
        BackendNotice::Event(Box::new(ThreadEvent::TurnStarted)),
        "turn started notice",
    );
    let drive = match input {
        TurnInput::Prompt { text, images } => {
            drive_run(
                session.prompt_with_images(&text, images),
                handle,
                cmd_rx,
                &mut turn,
                shutdown_after_run,
                live,
                state,
                notice_tx,
                harness_model,
                sessions_dir,
                &session_path,
            )
            .await
        }
        TurnInput::Continue => {
            drive_run(
                session.continue_(),
                handle,
                cmd_rx,
                &mut turn,
                shutdown_after_run,
                live,
                state,
                notice_tx,
                harness_model,
                sessions_dir,
                &session_path,
            )
            .await
        }
    };
    // A panic or a dropped future leaves the harness in an unknown state:
    // record it here, once, and refuse further submits until a restart.
    if let Some(reason) = &drive.panic {
        state.mark_degraded(reason);
    }
    if drive.abandoned {
        state.mark_degraded("the run future was dropped after the abort grace expired");
    }
    // The run's own error notice (a terminal-assistant failure already reached
    // the facade through the message adapter).
    if let Err(err) = &drive.result {
        send_notice(
            notice_tx,
            BackendNotice::Event(Box::new(ThreadEvent::Error(anyhow::anyhow!("{err:#}")))),
            "error notice",
        );
    }
    let outcome = outcome_of_drive(&drive);
    // ── commit (bounded, durable) ───────────────────────────────────────
    let facts = turn
        .commit_turn(outcome.clone(), drive.abort_requested)
        .await;
    tracing::debug!(
        cancelled = facts.cancelled,
        failed = facts.failed,
        steered = facts.steered.len(),
        stranded = facts.stranded.len(),
        held = facts.held,
        "turn committed"
    );
    // ── project (detached, derived) ─────────────────────────────────────
    spawn_projection(Projection {
        appender: session.journal_appender(),
        state: Arc::clone(state),
        sessions_dir: sessions_dir.to_path_buf(),
        session_path,
        notice_tx: notice_tx.clone(),
        fallback_cwd: cwd.to_path_buf(),
    });
    TurnSettlement {
        result: drive.result,
        outcome,
        abort_requested: drive.abort_requested,
        held: facts.held,
    }
}

/// Land the turn's open row (K4 discipline: bounded retry, a durable loss
/// record, one facade notice). A turn whose open row was lost still runs — its
/// close is what every reader waits for — but the loss is never silent.
async fn land_turn_start(
    appender: &Arc<JournalAppender>,
    state: &Arc<EngineState>,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
) {
    if let Err(err) = append_typed_resilient(appender, "turn_start", turn_start_payload()).await {
        if let Some(row) = record_journal_loss(appender, "turn_start", &err).await {
            lock_mirror(&state.pending_journal).push(row);
        }
        send_notice(
            notice_tx,
            BackendNotice::Event(Box::new(ThreadEvent::Error(anyhow::anyhow!(
                "journal append permanently failed for `turn_start`: {err:#}; the entry was dropped"
            )))),
            "error notice",
        );
    }
}

/// The turn's verdict from the drive loop's supervision flags.
pub(super) fn outcome_of_drive(drive: &DriveOutcome) -> RunOutcome {
    if let Some(message) = &drive.panic {
        return RunOutcome::Failed {
            terminal: Some(message.clone()),
            cause: FailureCause::Panic,
        };
    }
    if drive.abandoned {
        return RunOutcome::Aborted;
    }
    derive_outcome(&drive.result, drive.abort_requested)
}

// ── the projection ──────────────────────────────────────────────────────────

/// Everything a projection pass reads. The journal handle is the only read
/// face — never the session borrow the run owned, so a projection can outlive
/// the run by construction.
pub(super) struct Projection {
    pub(super) appender: Arc<JournalAppender>,
    pub(super) state: Arc<EngineState>,
    pub(super) sessions_dir: PathBuf,
    pub(super) session_path: PathBuf,
    pub(super) notice_tx: mpsc::UnboundedSender<BackendNotice>,
    /// The session's assembly cwd: the projection's fallback when the chain
    /// carries no `cwd_change` row.
    pub(super) fallback_cwd: PathBuf,
}

impl Clone for Projection {
    fn clone(&self) -> Self {
        Self {
            appender: Arc::clone(&self.appender),
            state: Arc::clone(&self.state),
            sessions_dir: self.sessions_dir.clone(),
            session_path: self.session_path.clone(),
            notice_tx: self.notice_tx.clone(),
            fallback_cwd: self.fallback_cwd.clone(),
        }
    }
}

/// Schedule one detached projection pass. Single-flight: a request arriving
/// while one runs raises the dirty flag and returns — the running pass reruns
/// once when it finishes, so the mirrors eventually reflect the newest chain
/// without ever stacking passes.
pub(super) fn spawn_projection(projection: Projection) {
    if crate::runtime::try_handle().is_none() {
        // No runtime (a test that never initialized one): the mirrors stay as
        // they are. The turn itself is already durably closed, which is the
        // only invariant this path must keep.
        tracing::debug!("no runtime available; skipping the detached turn projection");
        return;
    }
    let state = Arc::clone(&projection.state);
    {
        let mut flight = lock_mirror(&state.projection);
        if flight.running {
            flight.dirty = true;
            return;
        }
        flight.running = true;
    }
    crate::runtime::handle().spawn(async move {
        loop {
            // A panicked pass is a skipped refresh, never a dead slot: the
            // flight flag is cleared below whatever the pass did, so the next
            // commit re-runs it. The mirror locks may be poisoned by the same
            // panic, hence `into_inner`.
            let pass = std::panic::AssertUnwindSafe(project_turn(&projection))
                .catch_unwind()
                .await;
            if let Err(payload) = pass {
                tracing::error!(
                    payload = %panic_message(&payload),
                    "turn projection pass panicked; the slot stays usable"
                );
            }
            let mut flight = state
                .projection
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if !flight.dirty {
                flight.running = false;
                return;
            }
            flight.dirty = false;
        }
    });
}

/// A panic payload as text, for the projection's error line.
fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_string()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

/// Test seam: the next projection pass panics before touching any mirror, so
/// a test can prove the slot recovers.
#[cfg(test)]
pub(super) static PROJECTION_PANIC: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
pub(super) static PROJECTION_PANIC_MID: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
pub(super) fn panic_next_projection() {
    PROJECTION_PANIC.store(true, std::sync::atomic::Ordering::SeqCst);
}

#[cfg(test)]
pub(super) fn panic_next_projection_mid_pass() {
    PROJECTION_PANIC_MID.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// One projection pass: derived state only, discardable at any point.
///
/// Monotone by journal seq: the pass records the chain's cursor when it starts
/// reading and drops any mirror it derived if the chain moved on meanwhile —
/// derived data never rolls back authority.
pub(super) async fn project_turn(projection: &Projection) {
    #[cfg(test)]
    if PROJECTION_PANIC.swap(false, Ordering::SeqCst) {
        panic!("projection test panic");
    }
    let appender = &projection.appender;
    let state = &projection.state;
    let sessions_dir = &projection.sessions_dir;
    let session_path = &projection.session_path;
    // Parked persistence drains FIRST: the rows belong to the mirror this pass
    // is about to build, and their appends would otherwise look like foreign
    // progress to the monotone guard below.
    drain_parked_ui_notes(projection).await;
    #[cfg(test)]
    if PROJECTION_PANIC_MID.swap(false, Ordering::SeqCst) {
        // Poison `notes` by panicking while holding its guard: the next pass
        // must recover through `lock_mirror`, not panic again.
        let _guard = lock_mirror(&projection.state.notes);
        panic!("projection mid-pass test panic");
    }
    drain_parked_journal(projection).await;
    let seq = appender.storage().journal_cursor().await;
    let entries = match appender.build_context_entries().await {
        Ok(entries) => entries,
        Err(err) => {
            tracing::warn!(error = %err, "projection: context entries unavailable; mirrors left as-is");
            return;
        }
    };
    // The sticky cwd may have moved during the turn (a tool call with an
    // explicit `cwd`, or a `cd` inside a command): report one `CwdChanged` per
    // durable move so the facade mirror and the UI track the session tail.
    let projected = entries
        .iter()
        .rev()
        .find_map(|entry| match entry {
            Event::CwdChange { cwd, .. } => Some(PathBuf::from(cwd)),
            _ => None,
        })
        .unwrap_or_else(|| projection.fallback_cwd.clone());
    let projected_str = projected.to_string_lossy().into_owned();
    let last = lock_mirror(&state.last_cwd_note).clone();
    if last.as_deref() != Some(projected_str.as_str()) {
        *lock_mirror(&state.last_cwd_note) = Some(projected_str.clone());
        send_notice(
            &projection.notice_tx,
            BackendNotice::Event(Box::new(ThreadEvent::CwdChanged {
                path: projected_str,
            })),
            "cwd changed notice",
        );
    }
    // ── history mirror ──────────────────────────────────────────────────
    let (display, notes) = display_from_entries(&entries, sessions_dir, session_path).await;
    if journal_advanced(appender, seq).await {
        tracing::debug!("history mirror discarded: the journal moved past the projection's cursor");
        return;
    }
    *lock_mirror(&state.history) = display;
    *lock_mirror(&state.notes) = notes;
    state.notes_gen.fetch_add(1, Ordering::SeqCst);
    // ── usage mirror ────────────────────────────────────────────────────
    if !sync_usage(appender, state, Some(seq)).await {
        return;
    }
    // ── session list (detached by its own spawn) ────────────────────────
    spawn_session_list_refresh(sessions_dir, state);
}

/// Whether the journal moved past the projection's starting cursor: the
/// derived result then describes a chain that no longer is, so it is dropped
/// (the dirty flag schedules the rerun).
pub(super) async fn journal_advanced(appender: &Arc<JournalAppender>, seq: u64) -> bool {
    appender.storage().journal_cursor().await > seq
}

/// Drain the UI notes parked while the run owned the session, persisting each
/// through the shared journal writer (the writer serializes appends, so this
/// cannot fork the chain).
async fn drain_parked_ui_notes(projection: &Projection) {
    let parked = std::mem::take(&mut *lock_mirror(&projection.state.pending_ui_notes));
    for record in parked {
        let _ = persist_ui_note(
            &projection.appender,
            &projection.state,
            &projection.notice_tx,
            &record,
        )
        .await;
    }
}

/// Drain the journal rows parked by the mid-run serializer, running the same
/// K4 fail-loud discipline as every typed-append face: bounded retries, then a
/// durable loss record and one facade notice.
async fn drain_parked_journal(projection: &Projection) {
    let parked = std::mem::take(&mut *lock_mirror(&projection.state.pending_journal));
    if parked.is_empty() {
        return;
    }
    let mut loss_notified = false;
    for (kind, payload) in parked {
        let kind_label = kind.clone();
        if let Err(err) = append_typed_resilient(&projection.appender, &kind, payload).await {
            if let Some(row) = record_journal_loss(&projection.appender, &kind, &err).await {
                lock_mirror(&projection.state.pending_journal).push(row);
            }
            if kind_label != "error" && !loss_notified {
                loss_notified = true;
                send_notice(
                    &projection.notice_tx,
                    BackendNotice::Event(Box::new(ThreadEvent::Error(anyhow::anyhow!(
                        "journal append permanently failed for `{kind_label}` at settle: {err:#}; the entry was dropped"
                    )))),
                    "error notice",
                );
            }
        }
    }
}
