//! Split out of `engine::mod` verbatim (pure movement; the items keep their
//! semantics — only the module boundary and visibility changed).

use super::*;

/// Bind the orchestrators to a freshly built session: the monitor steerer
/// lands events in the session's steering queue and the background manager
/// subscribes to the session's lifecycle.
pub(super) fn attach_orchestrators(session: &mut AgentSession, orch: &SessionOrchestrators) {
    let handle = session.handle();
    orch.monitor.attach(&handle);
    orch.background.attach(session);
}
pub(super) fn steer_message(id: String, text: String, images: Vec<ContentBlock>) -> AgentMessage {
    let mut content = vec![ContentBlock::Text {
        text,
        signature: None,
    }];
    // TS `createUserMessage(text, images)` parity: image blocks ride the
    // steered user message behind the text.
    content.extend(images);
    // S3 stable-id: carry the client `Steer` message id as the durable row
    // id so the injected `user` journal row is keyed by the id the client
    // already correlates its echo against.
    AgentMessage::User {
        content,
        timestamp: chrono::Utc::now(),
        id: Some(id),
    }
}

/// Merge a freshly appended UI note into the engine mirror at the tail and
/// record its position over the mapped-message count — the same base
/// `merge_positioned_notes` re-derives on live ticks.
pub(super) fn mirror_ui_note(state: &Arc<EngineState>, record: UiNoteRecord) {
    let after_message = state
        .history
        .lock()
        .unwrap()
        .iter()
        .filter(|entry| matches!(entry, HistoryEntry::Message(_)))
        .count();
    state
        .history
        .lock()
        .unwrap()
        .push(HistoryEntry::Note(record.clone()));
    state.notes.lock().unwrap().push(PositionedNote {
        note: record,
        after_message,
    });
    state.notes_gen.fetch_add(1, Ordering::SeqCst);
}

/// Serialize and append one UI note as a `custom` entry at the session leaf.
pub(super) async fn persist_ui_note(
    session: &AgentSession,
    state: &EngineState,
    notice_tx: &tokio::sync::mpsc::UnboundedSender<BackendNotice>,
    record: &UiNoteRecord,
) -> bool {
    let data = match serde_json::to_value(record) {
        Ok(value) => Some(value),
        Err(err) => {
            // A None payload renders as a ghost entry on reload; name the
            // impossible case loudly instead of silently dropping the card.
            tracing::warn!(error = %err, "UI note serialization failed");
            None
        }
    };
    // K9/K4 symmetry: the UI-note append is a typed-append face like any
    // other (plan/approval cards ride it) — bounded retries, then the
    // durable loss record (parked for the settle/idle drains when the
    // storage itself is down) and one facade notice. The former warn-only
    // path swallowed permanent losses: the card vanished on reload with no
    // journal trace. No mid-run cancel leg — a UI note is not transcript,
    // so its loss never voids the turn.
    let mut last_err = None;
    for attempt in 1..=TYPED_APPEND_ATTEMPTS {
        match session
            .append_custom(UI_NOTE_CUSTOM_TYPE, data.clone())
            .await
        {
            Ok(_) => return true,
            Err(err) => {
                tracing::warn!(%err, attempt, "UI note journal append failed");
                last_err = Some(err);
                if attempt < TYPED_APPEND_ATTEMPTS {
                    tokio::time::sleep(TYPED_APPEND_RETRY_DELAY * attempt).await;
                }
            }
        }
    }
    let err = last_err.expect("the attempt loop ran at least once");
    let appender = session.journal_appender();
    if let Some(row) = record_journal_loss(&appender, "ui_note", &err).await {
        state.pending_journal.lock().unwrap().push(row);
    }
    let _ = notice_tx.send(BackendNotice::Event(Box::new(ThreadEvent::Error(
        anyhow::anyhow!(
            "journal append permanently failed for `ui_note`: {err:#}; the entry was dropped"
        ),
    ))));
    false
}

/// Merge or strip a browser suite's tool names into an active-tool set.
/// `enable` appends the suite's names (deduplicated); disable removes them.
/// Pure — unit-tested without a session.
pub(super) fn toggle_browser_suite_names(
    mut names: Vec<String>,
    suite_names: &[&str],
    enable: bool,
) -> Vec<String> {
    if enable {
        for name in suite_names {
            if !names.iter().any(|n| n == name) {
                names.push(name.to_string());
            }
        }
    } else {
        names.retain(|n| !suite_names.contains(&n.as_str()));
    }
    names
}

/// Toggle a browser tool suite against the session's authoritative active-tool
/// set. Reads the current set from the session, merges or strips the suite's
/// names, and persists via `set_active_tools`.
pub(super) async fn apply_browser_suite(
    session: &mut AgentSession,
    suite: BrowserSuite,
    enable: bool,
) {
    // `None` means the full mounted set is active.
    let current = session
        .active_tool_names()
        .unwrap_or_else(|| session.tools());
    let names = toggle_browser_suite_names(current, suite.tool_names(), enable);
    if let Err(err) = session.set_active_tools(names).await {
        tracing::warn!(error = %err, "failed to toggle browser tool suite");
    }
}

/// The opt-in suites whose tool names are all present in a resolved
/// active-tool set; a suite is active only when fully selected.
pub(super) fn project_browser_suites(active: &[String]) -> Vec<BrowserSuite> {
    [BrowserSuite::ChromeUse, BrowserSuite::WebExplore]
        .into_iter()
        .filter(|suite| {
            suite
                .tool_names()
                .iter()
                .all(|name| active.iter().any(|n| n == name))
        })
        .collect()
}

/// The shared mid-run journal writer (`AgentSession::journal_appender`): the
/// same session `Arc` the persistence middleware holds, so a mid-run append
/// is linearized by the session's append lock and broadcasts to followers
/// like any other append.
pub(super) type JournalAppender =
    manox_harness::session::Session<manox_harness::session::jsonl::JsonlSessionStorage>;

/// The user message a prompt's text and images construct — the same shape
/// `prompt_input`'s batch entry carries (`[Text, images...]`), so the
/// middleware's content-match skip recognizes the run's announced message
/// as the already-persisted one.
pub(super) fn prompt_user_message(text: &str, images: &[ContentBlock]) -> AgentMessage {
    let mut content = Vec::with_capacity(images.len() + 1);
    content.push(ContentBlock::Text {
        text: text.to_string(),
        signature: None,
    });
    content.extend(images.iter().cloned());
    AgentMessage::User {
        content,
        timestamp: chrono::Utc::now(),
        // The prompt row's durable id is pinned via K5 `accepted_entry`
        // (persist_prompt_user_entry), not carried on the message.
        id: None,
    }
}

/// K5: resolve the prompt's user-entry persistence BEFORE the run starts.
///
/// A Submit accepted at the gateway arrives already persisted
/// (`accepted_entry` = the journal entry id appended at acceptance, origin
/// pinned on it); a queued Submit persists here, at drain — still ahead of
/// the run, so ahead of model-visible ("model-visible ⟺ logged"). Both arm
/// the middleware skip (entry id + accepted content) so the run's own user
/// `MessageEnd` records the existing entry instead of appending a
/// duplicate; jsonl's duplicate-entry-id refusal stays the backstop, never
/// the normal path. The pin and any drain-time append carry the
/// POST-expansion text (the K5 edge: the run announces the expanded
/// shape); the acceptance-side persistence expands identically. An origin-less prompt (an internally-driven `run()`
/// turn, a goal/monitor seed) keeps the legacy flow: the middleware
/// persists its user message at announce time.
pub(super) async fn persist_prompt_user_entry(
    session: &AgentSession,
    text: &str,
    images: &[ContentBlock],
    origin_rpc: Option<String>,
    accepted_entry: Option<String>,
) -> Result<Option<String>, anyhow::Error> {
    let appender = session.journal_appender();
    // A stale pin no run consumed (one died before announcing its user
    // message) must not leak the middleware skip into this turn.
    appender.clear_accepted_user_entry();
    if accepted_entry.is_none() && origin_rpc.is_none() {
        return Ok(None);
    }
    // K5 edge: the pin carries the POST-expansion content (the shape the
    // run announces) and the queued-submit drain persistence logs the
    // expanded text the model actually sees. Run-time input-hook
    // transforms remain a residual edge: their mismatch double-journals
    // acceptably (the raw pin is the user intent, the appended announce
    // the model-visible truth).
    let expanded = manox_harness::harness::expand_prompt_with(session.resources(), text);
    let message = prompt_user_message(&expanded, images);
    let content = match &message {
        AgentMessage::User { content, .. } => {
            serde_json::to_value(content).unwrap_or(serde_json::Value::Null)
        }
        _ => unreachable!("prompt_user_message builds a user message"),
    };
    let entry_id = match accepted_entry {
        Some(id) => id,
        None => {
            // Durable: a still-deferred session (no file yet) puts the
            // accepted text on disk here, not at the first assistant
            // message.
            appender.append_message_durable(message, origin_rpc).await?
        }
    };
    appender.pin_accepted_user_entry(entry_id.clone(), content);
    Ok(Some(entry_id))
}

/// K4 (§C.3, L3): the typed-append discipline shared by every journal write
/// face. A transient failure retries a bounded number of times with a short
/// backoff; a permanent failure must never drop the row silently — callers
/// run the fail-loud tail ([`record_journal_loss`], a facade notice, and,
/// mid-run, turn cancellation), mirroring the persistence middleware's
/// message-append rule where a failure aborts the whole run.
pub(super) const TYPED_APPEND_ATTEMPTS: u32 = 3;
pub(super) const TYPED_APPEND_RETRY_DELAY: std::time::Duration =
    std::time::Duration::from_millis(50);

pub(super) async fn append_typed_resilient(
    appender: &JournalAppender,
    kind: &str,
    payload: serde_json::Value,
) -> Result<String, anyhow::Error> {
    let mut last_err = None;
    for attempt in 1..=TYPED_APPEND_ATTEMPTS {
        match appender.append_typed(kind, payload.clone()).await {
            Ok(id) => return Ok(id),
            Err(err) => {
                tracing::warn!(%err, kind, attempt, "typed journal append failed");
                last_err = Some(err);
                if attempt < TYPED_APPEND_ATTEMPTS {
                    tokio::time::sleep(TYPED_APPEND_RETRY_DELAY * attempt).await;
                }
            }
        }
    }
    Err(last_err.expect("the attempt loop ran at least once"))
}

/// The K4 loss record for a permanently failed typed append: a durable
/// `error` entry naming the dropped kind and reason. Best-effort — when the
/// storage itself is down the record cannot land either, so it is handed
/// back for the caller to park (`pending_journal`) and the settle/idle drain
/// persists it once the storage recovers. `error` rows never spawn a
/// compensation of their own: that breaks the tap feedback loop
/// (`ThreadEvent::Error` → tap → `AppendJournal("error")` → loss → …) and
/// keeps a dead storage from amplifying one failure into a notice storm.
pub(super) async fn record_journal_loss(
    appender: &JournalAppender,
    kind: &str,
    err: &anyhow::Error,
) -> Option<(String, serde_json::Value)> {
    if kind == "error" {
        tracing::error!(%err, "journal append failed for an error record; dropping it");
        return None;
    }
    let compensation = (
        "error".to_string(),
        serde_json::json!({
            "message": format!(
                "journal append permanently failed for `{kind}`: {err:#} — the entry was dropped"
            ),
        }),
    );
    match appender
        .append_typed(&compensation.0, compensation.1.clone())
        .await
    {
        Ok(_) => None,
        Err(_) => Some(compensation),
    }
}

/// Drive one session run to completion while still servicing mid-run
/// commands (abort/steer/cancel/shutdown) through the session handle.
/// Shared by user prompts, monitor idle-wakeups, and plan-approval seeds.
/// Returns the run result and whether an abort was requested.
///
/// While the run is in flight, a periodic tick refreshes the engine's
/// history mirror from the live transcript (`LiveHistory` notice) so a
/// thread switched back to mid-turn rebuilds from current progress.
#[allow(clippy::too_many_arguments)] // actor plumbing: each input is distinct session state
pub(super) fn session_builder(
    cwd: &Path,
    sessions_dir: &Path,
    runtime: &ModelRuntime,
    model: Option<&PiModel>,
    gate: &Arc<ApprovalGate>,
    question_gate: &Arc<crate::questions::UserQuestionGate>,
    plan: &Arc<crate::plan_mode::PlanSessionState>,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
    goal_bridge: Option<&Arc<crate::goal_tools::GoalBridge>>,
    granted_roots: &crate::granted_roots::GrantedRoots,
    thread_id: &str,
    parent_session: Option<&str>,
    bus: &Arc<crate::steer_bus::AgentBus>,
) -> (
    manox_harness::coding_agent::AgentSessionBuilder,
    SessionOrchestrators,
    crate::plan_mode::ReadOnlySubagentResolver,
) {
    let (tools, orchestrators, read_only_subagent) = build_tools(
        cwd,
        runtime,
        model,
        thread_id,
        gate,
        question_gate,
        plan,
        notice_tx,
        goal_bridge,
        granted_roots,
        bus,
    );
    // One observer wiring per orchestrator pair: the host task center sees
    // every producer lifecycle from this session (card snapshots + stop
    // routing), no per-open re-attach.
    crate::background_task::attach(
        Arc::clone(&orchestrators.monitor),
        Arc::clone(&orchestrators.background),
        notice_tx.clone(),
        thread_id.to_string(),
    );
    let default_active = default_active_tool_names(&tools);
    let mut builder = create_agent_session()
        .with_cwd(cwd.to_path_buf())
        .with_session_dir(sessions_dir.to_path_buf())
        .with_model_runtime(runtime.clone())
        .with_system_prompt_builder(manox_harness::prompt::captain_prompt_builder(
            manox_harness::prompt::CaptainConfig {
                cwd: cwd.to_path_buf(),
                today: chrono::Local::now().format("%Y-%m-%d").to_string(),
                skills: crate::skill::summaries_or_empty()
                    .into_iter()
                    .map(|s| manox_harness::prompt::SkillSummary {
                        name: s.name,
                        description: s.description,
                    })
                    .collect(),
            },
        ))
        .with_resources(instruction_resources(cwd))
        .with_tools(tools)
        .with_initial_active_tools(default_active);
    // Persist hashline snapshots under the manox config dir so an edit tag
    // survives an app restart, a session fork, or a worktree re-entry.
    if let Ok(config_dir) = crate::paths::manox_config_dir() {
        builder = builder.with_snapshot_dir(config_dir.join("hashline-snapshots"));
    }
    // Every session a host creates is tagged with its identity so each
    // host's session list stays disjoint, and with its owning thread id so
    // the sidebar groups one thread's sessions (base + worktree forks) into
    // a single row. A team worker additionally carries its leader's session
    // id: the link persists with the jsonl file, so the affiliation survives
    // restarts and outlives the in-memory team.
    builder = builder.with_metadata(session_metadata(thread_id, parent_session));

    if let Some(model) = model {
        builder = builder.with_model(model.clone());
    }
    (builder, orchestrators, read_only_subagent)
}
/// Adopt the session's own model after a restore: the reopened session
/// projects its persisted model onto the harness, and the actor's working
/// model plus the shared slot must follow so `Ready` and the title
/// scheduler all see the restored choice.
pub(super) fn adopt_session_model(
    session: &AgentSession,
    pi_model: &mut PiModel,
    state: &EngineState,
) {
    let restored = session.model().clone();
    *pi_model = restored.clone();
    *state.model.lock().unwrap() = Some(restored);
}

/// Register plan-mode extension hooks on a freshly built/restored session:
/// `BeforeAgentStart` injects the rendered plan-mode instructions every turn
/// while active; `ToolCall` enforces the read-only guarantee (plan-file
/// writes excepted). Both read through the shared [`PlanSessionState`].
pub(super) fn attach_plan_hooks(
    session: &mut AgentSession,
    plan: &Arc<crate::plan_mode::PlanSessionState>,
    cwd: &Path,
    read_only_subagent: crate::plan_mode::ReadOnlySubagentResolver,
) {
    session.on(
        manox_harness::harness::HookPoint::BeforeAgentStart,
        crate::plan_mode::injection_handler(Arc::clone(plan)),
    );
    let plans_dir = crate::paths::plans_dir().unwrap_or_else(|_| PathBuf::from(".manox/plans"));
    session.on(
        manox_harness::harness::HookPoint::ToolCall,
        crate::plan_mode::gate_handler(
            Arc::clone(plan),
            plans_dir,
            cwd.to_path_buf(),
            read_only_subagent,
        ),
    );
}

/// Instruction-file resources for the session: the Claude Code-compatible
/// memory hierarchy (managed policy, `~/.claude/CLAUDE.md` + rules, the
/// per-directory chain down to the session cwd) loaded through
/// [`crate::claude_md`] and folded into the system prompt by the kernel
/// every turn (TS project-instruction semantics). Skills/templates stay
/// empty here — manox skills ride the `manox_agent::skill` registry instead.
pub(super) fn instruction_resources(cwd: &Path) -> manox_harness::harness::HarnessResources {
    let set = crate::claude_md::load(cwd, &crate::settings::claude_md_load_context());
    let context_files = set
        .eager
        .iter()
        .map(|src| manox_harness::harness::ContextFile {
            name: src
                .path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "CLAUDE.md".to_string()),
            location: src.path.display().to_string(),
            content: src.content.clone(),
        })
        .collect();
    manox_harness::harness::HarnessResources {
        skills: Vec::new(),
        prompt_templates: Vec::new(),
        context_files,
    }
}

/// Register the plugin-lifecycle hook bridges (PreToolUse / PostToolUse
/// fire-and-forget shell-outs). Notification-only; never blocks a call.
pub(super) fn attach_plugin_hooks(session: &mut AgentSession, cwd: &Path) {
    session.on(
        manox_harness::harness::HookPoint::ToolCall,
        crate::plugin_hooks::pre_tool_call_handler(cwd.to_path_buf()),
    );
    session.on(
        manox_harness::harness::HookPoint::ToolResult,
        crate::plugin_hooks::post_tool_result_handler(cwd.to_path_buf()),
    );
}

/// Register the prefix-cache stability gate on a session: it observes every
/// provider request payload of this thread run and publishes
/// `ThreadEvent::PrefixStability` / `ThreadEvent::CacheInvalidation` on the
/// actor's notice channel. Observation is strictly read-only — the handler
/// returns its context untouched, so the model-visible bytes are unchanged.
/// One gate is one run's baseline: it is per-session state by construction.
pub(super) fn attach_prefix_gate(
    session: &mut AgentSession,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
    thread_id: &str,
) {
    crate::prefix_gate::attach_prefix_gate(session, notice_tx, thread_id);
}

#[allow(clippy::too_many_arguments)] // actor plumbing: each input is distinct session state
pub(super) async fn rebuild_session(
    session: &mut AgentSession,
    path: &Path,
    sessions_dir: &Path,
    runtime: &ModelRuntime,
    pi_model: &mut PiModel,
    state: &EngineState,
    fallback_cwd: &Path,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
    gate: &Arc<ApprovalGate>,
    plan: &Arc<crate::plan_mode::PlanSessionState>,
    goal_bridge: Option<&Arc<crate::goal_tools::GoalBridge>>,
    granted_roots: &crate::granted_roots::GrantedRoots,
    thread_id: &str,
    bus: &Arc<crate::steer_bus::AgentBus>,
) {
    // The old session is replaced (its Drop runs on the actor thread); it is
    // already idle when a switch happens, so nothing in-flight is lost. The
    // project cwd comes from the opened transcript's own header — reading the
    // one file, never a store-wide `list` (the #765 thread-switch stall).
    let repo = manox_harness::session::repository::SessionRepository::new(sessions_dir);
    let cwd = repo
        .info(path)
        .await
        .ok()
        .map(|info| PathBuf::from(info.cwd))
        .map(|cwd| {
            if cwd.as_os_str() == "/" {
                fallback_cwd.to_path_buf()
            } else {
                cwd
            }
        })
        .unwrap_or_else(|| fallback_cwd.to_path_buf());
    // Like the startup restore, a session swap passes no model override so
    // the opened session's own persisted model wins (TS `options.model >
    // restored model`); the actor adopts it right after the open.
    let (builder, orchestrators, read_only_subagent) = session_builder(
        &cwd,
        sessions_dir,
        runtime,
        None,
        gate,
        &state.question_gate,
        plan,
        notice_tx,
        goal_bridge,
        granted_roots,
        thread_id,
        None,
        bus,
    );
    match builder.open(path.to_path_buf()).await {
        Ok(mut s) => {
            attach_orchestrators(&mut s, &orchestrators);
            attach_plan_hooks(&mut s, plan, &cwd, read_only_subagent);
            // Session swaps (Open) must carry the
            // same plugin lifecycle hooks as fresh builds, or the swapped
            // session's PreToolUse/PostToolUse fire-and-forget shell-outs
            // never attach (write confinement is now in ApprovalGatedTool).
            attach_plugin_hooks(&mut s, &cwd);
            attach_prefix_gate(&mut s, notice_tx, thread_id);
            adopt_session_model(&s, pi_model, state);
            *session = s;
            // The rebuilt session owns a new storage: its own journal relay.
            spawn_journal_relay(session, &state.journal_tx);
            // K5: the rebuilt session is the acceptance-time writer.
            *state.current_appender.lock().unwrap() = Some(session.journal_appender());
            *state.current_resources.lock().unwrap() = Some(session.resources().clone());
        }
        Err(err) => {
            let _ = notice_tx.send(BackendNotice::Fatal(anyhow::anyhow!(
                "pi session open failed: {err}"
            )));
        }
    }
}
