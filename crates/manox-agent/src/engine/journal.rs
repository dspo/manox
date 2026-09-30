//! Engine journal plumbing: the durable-payload projection, the
//! per-thread engine routes, and the cold-append path.

use super::*;

/// Map a facade event to its durable journal entry (§C.2): `(wire kind,
/// payload)`. `None` means the event is not journaled — because it is
/// already persisted by its owning flow (`model_change`, `thinking_level`,
/// `cwd_change`, `compaction`, messages) or is snapshot-semantics
/// (`HistoryProgress/Restored`, `PlanReady`) or not in the vocabulary
/// (`PeerMessage`, steer bookkeeping — steer rides messages).
///
/// This is the single mapping point the notice tap consumes; payload keys
/// are the variant's camelCase serde names (envelope-key exclusivity §C.1:
/// handles are callId/agentId, never id).
pub(super) fn durable_journal_payload(ev: &ThreadEvent) -> Option<(String, serde_json::Value)> {
    use serde_json::json;
    let status_str = |status: &crate::thread::ToolCallStatus| {
        serde_json::to_value(status)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
    };
    let stop_reason_str = |reason: &Option<crate::language_model::StopReason>| {
        reason.map(|r| match r {
            crate::language_model::StopReason::EndTurn => "end_turn",
            crate::language_model::StopReason::MaxTokens => "max_tokens",
            crate::language_model::StopReason::ToolUse => "tool_use",
            crate::language_model::StopReason::Refusal => "refusal",
            crate::language_model::StopReason::Cancelled => "cancelled",
        })
    };
    Some(match ev {
        // ── lifecycle ────────────────────────────────────────────────────
        // The turn start stamps the owning process (§C.2 turnStart owner):
        // the journal is shared across processes, and a reader of an open
        // turn needs the owner's liveness to settle a dead turn on its
        // behalf without ever killing one another process is running.
        ThreadEvent::TurnStarted => (
            "turn_start".into(),
            json!({ "owner": manox_journal::TurnOwner::for_current_process() }),
        ),
        ThreadEvent::TurnFinished {
            cancelled,
            failed,
            stranded_steer_ids,
        } => (
            "turn_finish".into(),
            json!({
                "cancelled": cancelled,
                "failed": failed,
                "strandedSteerIds": stranded_steer_ids,
            }),
        ),
        ThreadEvent::Stop(reason) => (
            "stop".into(),
            json!({ "reason": stop_reason_str(&Some(*reason)) }),
        ),
        ThreadEvent::Retry {
            attempt,
            max_attempts,
            delay_secs,
            reason,
            detail,
        } => (
            "retry".into(),
            json!({
                "attempt": attempt,
                "maxAttempts": max_attempts,
                "delaySecs": delay_secs,
                "reason": reason,
                "detail": detail,
            }),
        ),
        ThreadEvent::Error(err) => ("error".into(), json!({ "message": format!("{err:#}") })),
        // ── streaming deltas ─────────────────────────────────────────────
        ThreadEvent::AgentText(text) => ("agent_text_delta".into(), json!({ "delta": text })),
        ThreadEvent::AgentThinking(text) => {
            ("agent_thinking_delta".into(), json!({ "delta": text }))
        }
        ThreadEvent::ToolCall {
            id,
            name,
            title,
            status,
            input,
        } => (
            "tool_call".into(),
            json!({
                "callId": id,
                "name": name,
                "title": title,
                "status": status_str(status)?,
                "input": input,
            }),
        ),
        ThreadEvent::ToolResult {
            id,
            output,
            is_error,
        } => (
            "tool_result".into(),
            json!({ "callId": id, "output": output, "isError": is_error }),
        ),
        ThreadEvent::ToolOutput { id, chunk } => (
            "tool_output_chunk".into(),
            json!({ "callId": id, "chunk": chunk }),
        ),
        ThreadEvent::SubagentStarted {
            id,
            subagent_type,
            description,
            child,
        } => (
            "subagent_child".into(),
            json!({
                "agentId": id,
                "event": {
                    "type": "started",
                    "subagentType": subagent_type,
                    "description": description,
                    "childId": child.0,
                },
            }),
        ),
        ThreadEvent::SubagentProgress {
            id,
            subagent_type,
            tool_uses,
            token_usage,
            latest_activity,
            status,
            ..
        } => (
            "subagent_progress".into(),
            json!({
                "agentId": id,
                "agentType": subagent_type,
                "toolUses": tool_uses,
                "tokenUsage": serde_json::to_value(token_usage).unwrap_or(serde_json::Value::Null),
                "latestActivity": latest_activity,
                "status": status_str(status)?,
            }),
        ),
        ThreadEvent::SubagentChild { id, child } => (
            "subagent_child".into(),
            json!({
                "agentId": id,
                "event": serde_json::to_value(child).unwrap_or(serde_json::Value::Null),
            }),
        ),
        // ── state changes (sidecar writes continue during migration; the
        //    journal entry is the future single truth, L10) ───────────────
        ThreadEvent::PermissionModeChanged { mode } => (
            "permission_mode_change".into(),
            // The closed kebab wire vocabulary (§C.2 `mode`), never the
            // Debug name: the replay fold, the sidecar cache, and the wire
            // projection all parse `from_wire`.
            json!({ "mode": mode.wire() }),
        ),
        ThreadEvent::PlanModeChanged { enabled } => {
            ("plan_mode_change".into(), json!({ "enabled": enabled }))
        }
        ThreadEvent::PlanUpdated { snapshot } => (
            "plan_update".into(),
            json!({ "snapshot": serde_json::to_value(snapshot).unwrap_or(serde_json::Value::Null) }),
        ),
        ThreadEvent::GoalChanged { goal } => (
            "goal".into(),
            json!({ "goal": serde_json::to_value(goal).unwrap_or(serde_json::Value::Null) }),
        ),
        ThreadEvent::TitleChanged { title } => ("title".into(), json!({ "title": title })),
        ThreadEvent::BrowserSuitesChanged { suites } => (
            "browser_suites".into(),
            json!({
                "suites": serde_json::to_value(suites)
                    .unwrap_or(serde_json::Value::Null)
            }),
        ),
        ThreadEvent::BackgroundTaskUpdated { snapshot } => (
            "background_task".into(),
            json!({ "snapshot": serde_json::to_value(snapshot).unwrap_or(serde_json::Value::Null) }),
        ),
        ThreadEvent::ToolCallAuthorization {
            id,
            tool_name,
            summary,
            input,
        } => (
            if tool_name == crate::tools::ASK_USER_QUESTION {
                "question".to_string()
            } else {
                "approval".to_string()
            },
            json!({
                "kind": "request",
                "authId": id,
                "payload": { "toolName": tool_name, "summary": summary, "input": input },
            }),
        ),
        ThreadEvent::CompactionStarted { tokens_before } => (
            "compaction_started".into(),
            json!({ "tokensBefore": tokens_before }),
        ),
        // ── metrics (low wire priority, still logged) ─────────────────────
        ThreadEvent::TokenUsageUpdated(usage) => (
            "metrics".into(),
            json!({
                "metricType": "token_usage",
                "data": serde_json::to_value(usage).unwrap_or(serde_json::Value::Null),
            }),
        ),
        ThreadEvent::PrefixStability {
            stability_pct,
            system_changed,
            tools_changed,
        } => (
            "metrics".into(),
            json!({
                "metricType": "prefix_stability",
                "data": { "stabilityPct": stability_pct, "systemChanged": system_changed, "toolsChanged": tools_changed },
            }),
        ),
        ThreadEvent::CacheInvalidation { reprocessed_tokens } => (
            "metrics".into(),
            json!({ "metricType": "cache_invalidation", "data": { "reprocessedTokens": reprocessed_tokens } }),
        ),
        ThreadEvent::SideCallMetricsUpdated(metrics) => (
            "metrics".into(),
            json!({
                "metricType": "side_call",
                "data": serde_json::to_value(metrics).unwrap_or(serde_json::Value::Null),
            }),
        ),
        ThreadEvent::MainCallMetricsUpdated(metric) => (
            "metrics".into(),
            json!({
                "metricType": "main_call",
                "data": serde_json::to_value(metric).unwrap_or(serde_json::Value::Null),
            }),
        ),
        // The plan-review edge is journal-first (§C.2): the review card's
        // durable authority is the `plan_review` row (replay + the AHP
        // translation both fold from it), so the proposal itself must journal
        // here rather than ride a sidecar that replay would forget.
        ThreadEvent::PlanReady {
            plan_file,
            title,
            content,
        } => (
            "plan_review".into(),
            json!({
                "state": "proposed",
                "planFile": plan_file,
                "title": title,
                "content": content,
            }),
        ),
        // Already durable through their owning flows / not journaled.
        ThreadEvent::ModelChanged { .. }
        | ThreadEvent::ReasoningEffortChanged { .. }
        | ThreadEvent::CwdChanged { .. }
        | ThreadEvent::Compaction { .. }
        | ThreadEvent::HistoryProgress
        | ThreadEvent::HistoryRestored
        | ThreadEvent::SteerInjected { .. }
        | ThreadEvent::UserRowLanded { .. }
        | ThreadEvent::PeerMessage { .. } => return None,
    })
}

// ── Thread → engine journal routing (K3) ───────────────────────────────────
//
// Kernel-level decision points that live outside the engine actor (the
// thread store's pin/archive writes) must still journal through the actor's
// serializer queue (`SessionCmd::AppendJournal` → the K4 fail-loud typed
// append face), so their entries are linearized against every other writer
// of the same session. When no live engine holds the thread, the row
// cold-appends through a freshly opened storage — which is only safe while
// no live storage writes the same file. The retirement protocol below makes
// the handoff structural:
//
// - A dispatch under the registry lock either SENDS into a non-retired
//   route (the row is then guaranteed to be appended by the actor: its
//   shutdown claim drains every queued row under the same lock), or sees a
//   retired/absent route and takes the cold path.
// - The actor retires its route and claims the queue in one lock hold at
//   shutdown, appends the claimed rows, closes the session, and only then
//   (on exit) removes the route — so a waiting cold append starts strictly
//   after the live storage stopped writing. One writer at a time, no lost
//   row.
pub(super) struct EngineRoute {
    tx: mpsc::UnboundedSender<SessionCmd>,
    /// Set (under the registry lock, together with the shutdown claim) when
    /// the actor broke its command loop. From then on dispatches never send
    /// into this route — they wait for its removal and cold-append.
    retired: Arc<std::sync::atomic::AtomicBool>,
}

pub(super) static ENGINE_ROUTES: std::sync::OnceLock<
    std::sync::Mutex<HashMap<String, EngineRoute>>,
> = std::sync::OnceLock::new();

pub(super) fn engine_routes() -> &'static std::sync::Mutex<HashMap<String, EngineRoute>> {
    ENGINE_ROUTES.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

pub(crate) fn register_engine_route(thread_id: &str, tx: &mpsc::UnboundedSender<SessionCmd>) {
    engine_routes().lock().unwrap().insert(
        thread_id.to_string(),
        EngineRoute {
            tx: tx.clone(),
            retired: Arc::new(AtomicBool::new(false)),
        },
    );
}

pub(super) fn unregister_engine_route(thread_id: &str, tx: &mpsc::UnboundedSender<SessionCmd>) {
    let mut routes = engine_routes().lock().unwrap();
    if routes
        .get(thread_id)
        .is_some_and(|route| route.tx.same_channel(tx))
    {
        routes.remove(thread_id);
    }
}

/// K3 shutdown protocol, step 1: retire this thread's registry route and
/// claim every `AppendJournal` row already queued. Runs under the registry
/// lock, so a concurrent [`dispatch_store_journal_row`] either sent before
/// this point (its row is claimed here and appended before close) or sees
/// the retirement (it waits for the route's removal, then cold-appends
/// after this actor's storage stopped writing). Non-journal rows queued at
/// shutdown are dropped: the actor has broken its command loop.
pub(super) fn retire_and_claim_journal_rows(
    thread_id: &str,
    cmd_rx: &mut mpsc::UnboundedReceiver<SessionCmd>,
) -> Vec<(String, serde_json::Value)> {
    let routes = engine_routes().lock().unwrap();
    if let Some(route) = routes.get(thread_id) {
        route.retired.store(true, Ordering::Relaxed);
    }
    let mut rows = Vec::new();
    while let Ok(cmd) = cmd_rx.try_recv() {
        if let SessionCmd::AppendJournal { kind, payload } = cmd {
            rows.push((kind, payload));
        }
    }
    rows
}

/// Land one journal row outside the actor (shutdown claim or cold append);
/// the K4 fail-loud discipline applies. A permanent loss records its
/// durable `error` entry where the storage allows and logs loudly
/// otherwise — no facade is left to notify on these paths.
pub(super) async fn append_row_fail_loud(
    appender: &JournalAppender,
    kind: String,
    payload: serde_json::Value,
) {
    if let Err(err) = append_typed_resilient(appender, &kind, payload).await {
        let _ = record_journal_loss(appender, &kind, &err).await;
        tracing::error!(%err, kind, "journal row could not land outside the actor loop");
    }
}

/// Route one store-level journal row (K3: pin/archive and any future
/// store-owned decision) to the thread's journal. Sends into the live
/// actor's serializer queue when one is registered; otherwise cold-appends
/// through a freshly opened storage once the file has no live writer.
/// Called synchronously by the store's dispatch; the waiting/cold paths run
/// on the agent runtime.
///
/// `session_path` is the store's cached journal-file path, needed for the
/// cold path; `None` with no live route logs and drops the row (a thread
/// that never materialized has no journal — its sidecar carries the flag
/// until the journal exists, the K2 fallback).
/// Returns whether the row was handed to a path that can still land it: a live
/// actor's queue, or a cold append against a file that exists. `false` means
/// there is nowhere for the row to go — no actor and no file — which the caller
/// must treat as "not persisted" rather than assuming success.
///
/// A cold append can still be dropped *later* (the file may be driven by
/// another process, whose lease this process cannot take); that case is logged
/// loudly at the seam and is deliberately not folded into this answer, because
/// deciding it here would mean taking the lease on a synchronous path that the
/// store's own lock is held across.
pub(crate) fn dispatch_store_journal_row(
    thread_id: String,
    session_path: Option<PathBuf>,
    kind: String,
    payload: serde_json::Value,
) -> bool {
    enum Fate {
        Queued,
        Wait,
        Cold,
    }
    let fate = {
        let routes = engine_routes().lock().unwrap();
        match routes.get(&thread_id) {
            Some(route) if !route.retired.load(Ordering::Relaxed) => {
                match route.tx.send(SessionCmd::AppendJournal {
                    kind: kind.clone(),
                    payload: payload.clone(),
                }) {
                    // The actor's shutdown claim covers this row: it drains
                    // every queued AppendJournal under the same lock that
                    // sets `retired`.
                    Ok(()) => Fate::Queued,
                    // Actor died without retiring (a Fatal early return):
                    // the route's removal is imminent — wait, then re-check.
                    Err(_) => Fate::Wait,
                }
            }
            // Retiring: the actor is appending its claimed rows / closing.
            Some(_) => Fate::Wait,
            None => Fate::Cold,
        }
    };
    match fate {
        Fate::Queued => true,
        Fate::Wait => {
            crate::runtime::handle().spawn(wait_then_cold_journal_append(
                thread_id,
                session_path,
                kind,
                payload,
            ));
            true
        }
        Fate::Cold => {
            let have_file = session_path.as_ref().is_some_and(|path| path.exists());
            if !have_file {
                tracing::debug!(
                    kind,
                    "no live route and no session file; the row has nowhere to land"
                );
            }
            crate::runtime::handle().spawn(cold_journal_append(session_path, kind, payload));
            have_file
        }
    }
}

/// Wait for a retiring route's removal (a successor engine re-registering
/// is re-checked and offered the row), then cold-append. Bounded: a hung
/// actor shutdown must not strand the decision forever — after the window
/// the row cold-appends best-effort with a loud log.
pub(super) async fn wait_then_cold_journal_append(
    thread_id: String,
    session_path: Option<PathBuf>,
    kind: String,
    payload: serde_json::Value,
) {
    for _ in 0..200u32 {
        enum Step {
            Done,
            KeepWaiting,
            Cold,
        }
        let step = {
            let routes = engine_routes().lock().unwrap();
            match routes.get(&thread_id) {
                Some(route) if !route.retired.load(Ordering::Relaxed) => {
                    // A successor engine took over the thread: its actor
                    // serializes the row against the same session file.
                    match route.tx.send(SessionCmd::AppendJournal {
                        kind: kind.clone(),
                        payload: payload.clone(),
                    }) {
                        Ok(()) => Step::Done,
                        Err(_) => Step::KeepWaiting,
                    }
                }
                Some(_) => Step::KeepWaiting,
                None => Step::Cold,
            }
        };
        match step {
            Step::Done => return,
            Step::Cold => break,
            Step::KeepWaiting => tokio::time::sleep(std::time::Duration::from_millis(25)).await,
        }
    }
    cold_journal_append(session_path, kind, payload).await;
}

/// B3 (review round 3): the cold-append serialization locks, keyed by
/// journal path. Each cold append opens a FRESH storage instance whose
/// `append_lock`/leaf/seq views are instance-scoped — two spawned
/// decisions on one engine-less thread (pin + archive is the canonical
/// pair, and a retiring actor's `Fate::Wait` queue funnels several)
/// raced: same parent, self-stamped seqs, a forked chain the load
/// validator accepts (sibling rows are legal) while the cursor keeps
/// only the file-last branch — the earlier decision silently left the
/// chain. Entries are never evicted: one small map entry per distinct
/// cold-appended session, process-lifetime.
pub(super) static COLD_APPEND_LOCKS: std::sync::OnceLock<
    std::sync::Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>,
> = std::sync::OnceLock::new();

pub(super) fn cold_append_lock(path: &Path) -> Arc<tokio::sync::Mutex<()>> {
    let mut map = COLD_APPEND_LOCKS
        .get_or_init(|| std::sync::Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    map.entry(path.to_path_buf())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

/// Cold journal append for a decision whose thread has no live engine (K3):
/// open the session file and land the typed row through the same storage
/// face a live actor uses (`append_typed`: parent selection + append lock +
/// seq stamp + journal broadcast). Safe because the registry protocol
/// guarantees no live storage writes this file while the route is absent.
/// A thread whose journal never materialized has no file: the row is
/// skipped loudly at debug — the sidecar carries the flag until the journal
/// exists (the K2 fallback).
///
/// Cross-process, the file may still have a live writer: another process's
/// actor holding the write lease. The lease is taken here too (short-held:
/// released with this function's return — the row-level serialization is
/// the storage's own `WriteFence`), and contention degrades exactly like
/// the K2 fallback: the row is skipped loudly and the sidecar keeps the
/// flag until this process or the other re-lands it.
pub(super) async fn cold_journal_append(
    session_path: Option<PathBuf>,
    kind: String,
    payload: serde_json::Value,
) {
    let Some(path) = session_path else {
        tracing::debug!(
            kind,
            "no session file to cold-append to; the sidecar carries the flag"
        );
        return;
    };
    if !path.exists() {
        tracing::debug!(kind, path = %path.display(), "session file does not exist; the sidecar carries the flag");
        return;
    }
    // Short-held write lease, spanning the whole append (released when
    // this function returns — the `_lease` binding lives to scope end). If
    // the session's live actor exists in THIS process, the registry join
    // makes this a no-op.
    let _lease = match crate::session_lease::acquire_async(&path).await {
        Ok(lease) => lease,
        Err(error) => {
            tracing::error!(%error, kind, path = %path.display(), "cold journal append skipped: the session is driven by another process; the sidecar carries the flag");
            return;
        }
    };
    // B3: the file-level serialization the per-instance append_lock cannot
    // give — held across the open (leaf/seq view) AND the append (stamp +
    // write + the v3 lazy migration), so concurrent cold appends to one
    // file run strictly one after another. The live-engine path needs no
    // such lock: its actor owns the file.
    let path_lock = cold_append_lock(&path);
    let _guard = path_lock.lock().await;
    let storage = match manox_harness::session::jsonl::JsonlSessionStorage::open(&path).await {
        Ok(storage) => storage,
        Err(err) => {
            tracing::error!(%err, kind, path = %path.display(), "cold journal append could not open the session file");
            return;
        }
    };
    let session = manox_harness::session::Session::new(storage);
    append_row_fail_loud(&session, kind, payload).await;
}
