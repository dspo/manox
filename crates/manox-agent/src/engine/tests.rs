//! Engine tests. The modules under test re-export their items through
//! `engine::mod`, so `use super::*` reaches everything.

use super::*;

#[test]
fn default_active_tool_names_excludes_browser_suites() {
    // The default active set keeps every non-browser tool and drops both
    // opt-in suites, so browser tools never ride the default system prompt.
    let tools: Vec<Arc<dyn PiAgentTool>> = vec![
        Arc::new(crate::chrome_use::ChromeUseOpenTool) as Arc<dyn PiAgentTool>,
        Arc::new(crate::web_tools::WebExploreOpenTool::new(
            tokio::sync::mpsc::unbounded_channel().0,
        )),
    ];
    let default_names = default_active_tool_names(&tools);
    assert!(default_names.is_empty(), "{default_names:?}");
}

#[test]
fn toggle_browser_suite_enable_merges_into_default_set() {
    // P0 regression: activating a suite against the default set must ADD
    // the suite's tools, not replace the defaults. Starting from the
    // default (non-browser) set, enabling ChromeUse yields defaults + the
    // ChromeUse suite — the defaults survive.
    let defaults = vec!["Read".to_string(), "Bash".to_string(), "Edit".to_string()];
    let merged =
        toggle_browser_suite_names(defaults.clone(), BrowserSuite::ChromeUse.tool_names(), true);
    // Every default survives.
    for name in &defaults {
        assert!(merged.contains(name), "default {name} lost: {merged:?}");
    }
    // Every ChromeUse tool is present.
    for name in BrowserSuite::ChromeUse.tool_names() {
        assert!(
            merged.iter().any(|n| n == name),
            "suite tool {name} missing: {merged:?}"
        );
    }
    assert_eq!(
        merged.len(),
        defaults.len() + BrowserSuite::ChromeUse.tool_names().len()
    );
}

#[test]
fn toggle_browser_suite_enable_is_idempotent() {
    let once = toggle_browser_suite_names(
        vec!["Read".to_string()],
        BrowserSuite::WebExplore.tool_names(),
        true,
    );
    let twice =
        toggle_browser_suite_names(once.clone(), BrowserSuite::WebExplore.tool_names(), true);
    assert_eq!(once, twice, "re-enabling must not duplicate");
}

#[test]
fn toggle_browser_suite_disable_strips_only_that_suite() {
    // Start with defaults + both suites; disabling ChromeUse removes only
    // the ChromeUse tools, leaving defaults + WebExplore intact.
    let mut names = vec!["Read".to_string(), "Bash".to_string()];
    names = toggle_browser_suite_names(names, BrowserSuite::ChromeUse.tool_names(), true);
    names = toggle_browser_suite_names(names, BrowserSuite::WebExplore.tool_names(), true);
    let stripped = toggle_browser_suite_names(names, BrowserSuite::ChromeUse.tool_names(), false);
    assert!(stripped.contains(&"Read".to_string()));
    assert!(stripped.contains(&"Bash".to_string()));
    for name in BrowserSuite::ChromeUse.tool_names() {
        assert!(!stripped.iter().any(|n| n == name), "{name} should be gone");
    }
    for name in BrowserSuite::WebExplore.tool_names() {
        assert!(stripped.iter().any(|n| n == name), "{name} should survive");
    }
}

#[test]
fn project_browser_suites_requires_the_full_suite_active() {
    // A suite projects as active only when every one of its tool names is
    // selected; the browser-free default set projects nothing.
    let defaults = vec!["Read".to_string(), "Bash".to_string()];
    assert!(project_browser_suites(&defaults).is_empty());

    let partial: Vec<String> = BrowserSuite::ChromeUse.tool_names()[..3]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert!(project_browser_suites(&partial).is_empty());

    let mut full = defaults.clone();
    full.extend(
        BrowserSuite::ChromeUse
            .tool_names()
            .iter()
            .map(|s| s.to_string()),
    );
    assert_eq!(project_browser_suites(&full), vec![BrowserSuite::ChromeUse]);

    full.extend(
        BrowserSuite::WebExplore
            .tool_names()
            .iter()
            .map(|s| s.to_string()),
    );
    assert_eq!(
        project_browser_suites(&full),
        vec![BrowserSuite::ChromeUse, BrowserSuite::WebExplore]
    );
}

#[test]
fn session_metadata_tags_host_thread_and_optional_team_parent() {
    let tagged = session_metadata("thread-1", Some("leader-1"));
    assert_eq!(tagged["host"], crate::host::current().slug());
    assert_eq!(tagged["thread"], "thread-1");
    assert_eq!(tagged["team"]["parent"], "leader-1");

    let plain = session_metadata("thread-1", None);
    assert_eq!(plain["host"], crate::host::current().slug());
    assert_eq!(plain["thread"], "thread-1");
    assert!(plain.get("team").is_none(), "no team key without a parent");
}

#[tokio::test]
async fn permission_mode_sidecar_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let session = dir.path().join("sess-1.jsonl");

    // Fresh session: no sidecar -> default.
    assert_eq!(
        load_approval_mode(dir.path(), &session).await,
        PermissionMode::WorkspaceWrite
    );

    write_approval_mode_sidecar(dir.path(), &session, PermissionMode::DangerFullAccess)
        .await
        .unwrap();
    assert_eq!(
        load_approval_mode(dir.path(), &session).await,
        PermissionMode::DangerFullAccess
    );

    write_approval_mode_sidecar(dir.path(), &session, PermissionMode::ReadOnly)
        .await
        .unwrap();
    assert_eq!(
        load_approval_mode(dir.path(), &session).await,
        PermissionMode::ReadOnly
    );
}

#[tokio::test]

async fn attach_registry_displays_restores_sidecar_compact_forms() {
    let dir = tempfile::tempdir().unwrap();
    let session = dir.path().join("sess-display.jsonl");
    let meta = manox_harness::session_meta::SessionMeta {
        registry_displays: [
            (0usize, "/gitwork:deliver fast".to_string()),
            (2usize, "/healthz".to_string()),
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    manox_harness::session_meta::save(dir.path(), &session, &meta)
        .await
        .unwrap();

    let displays = load_registry_displays(dir.path(), &session).await;
    // Ordinals count user prompts only: a tool result (user role, tool
    // provenance) and assistant turns must not consume one.
    let mut history: Vec<HistoryEntry> = vec![
        Message::user("expanded macro body".to_string()), // ordinal 0
        Message::user("plain turn".to_string()),          // ordinal 1
        Message::user_with_content(vec![MessageContent::ToolResult(
            crate::language_model::LanguageModelToolResult {
                tool_use_id: "tu_1".into(),
                tool_name: "Read".into(),
                is_error: false,
                content: "ok".into(),
            },
        )]),
        Message::assistant(vec![MessageContent::Text("reply".into())]),
        Message::user("expanded skill body".to_string()), // ordinal 2
    ]
    .into_iter()
    .map(HistoryEntry::Message)
    .collect();
    attach_registry_displays(&mut history, &displays);

    let ui = |ix: usize| match &history[ix] {
        HistoryEntry::Message(m) => m.ui.as_ref().and_then(|ui| ui.display_text.clone()),
        _ => None,
    };
    let no_ui = |ix: usize| match &history[ix] {
        HistoryEntry::Message(m) => m.ui.is_none(),
        _ => false,
    };
    assert_eq!(ui(0).as_deref(), Some("/gitwork:deliver fast"));
    assert!(no_ui(1), "plain turn keeps no display text");
    assert!(no_ui(2), "tool result never consumes a display ordinal");
    assert!(no_ui(3), "assistant turns never get display text");
    assert_eq!(ui(4).as_deref(), Some("/healthz"));
}

#[tokio::test]
async fn attach_user_attributions_restores_sidecar_authorship() {
    let dir = tempfile::tempdir().unwrap();
    let session = dir.path().join("sess-author.jsonl");
    let meta = manox_harness::session_meta::SessionMeta {
        user_attributions: [
            (
                0usize,
                manox_harness::session_meta::UserAttributionMeta {
                    author: "lead".into(),
                    peer: false,
                    display_text: None,
                },
            ),
            (
                1usize,
                manox_harness::session_meta::UserAttributionMeta {
                    author: "Sailor".into(),
                    peer: true,
                    display_text: Some("unwrapped body".into()),
                },
            ),
            // Beyond the transcript's ordinals: tolerated, attaches nowhere.
            (
                5usize,
                manox_harness::session_meta::UserAttributionMeta {
                    author: "lead".into(),
                    peer: false,
                    display_text: None,
                },
            ),
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    manox_harness::session_meta::save(dir.path(), &session, &meta)
        .await
        .unwrap();

    let attributions = load_user_attributions(dir.path(), &session).await;
    // Same ordinal convention as the display forms: tool results and
    // assistant turns never consume one.
    let mut history: Vec<HistoryEntry> = vec![
        Message::user("plan seed".to_string()), // ordinal 0
        Message::assistant(vec![MessageContent::Text("ok".into())]),
        Message::user_with_content(vec![MessageContent::ToolResult(
            crate::language_model::LanguageModelToolResult {
                tool_use_id: "tu_1".into(),
                tool_name: "Read".into(),
                is_error: false,
                content: "ok".into(),
            },
        )]),
        Message::user("[from Sailor]: wrapped".to_string()), // ordinal 1
    ]
    .into_iter()
    .map(HistoryEntry::Message)
    .collect();
    attach_user_attributions(&mut history, &attributions);

    let ui_at = |ix: usize| match &history[ix] {
        HistoryEntry::Message(m) => m.ui.clone(),
        _ => None,
    };
    let seed = ui_at(0).expect("seed carries attribution");
    assert_eq!(seed.author, Some(crate::message::MessageAuthor::Lead));
    assert!(!seed.peer);
    assert!(ui_at(1).is_none(), "assistant turns stay unattributed");
    let peer = ui_at(3).expect("peer delivery carries attribution");
    assert_eq!(
        peer.author,
        Some(crate::message::MessageAuthor::Agent("Sailor".into()))
    );
    assert!(peer.peer);
    assert_eq!(
        peer.display_text.as_deref(),
        Some("unwrapped body"),
        "the send-time display form survives the sidecar round-trip"
    );
    assert!(
        ui_at(2).is_none(),
        "tool results never consume an ordinal nor carry attribution"
    );
}

#[tokio::test]
async fn clear_user_chrome_drops_sidecar_ordinals() {
    let dir = tempfile::tempdir().unwrap();
    let session = dir.path().join("sess-display-clear.jsonl");
    let meta = manox_harness::session_meta::SessionMeta {
        registry_displays: [(0usize, "/gitwork:deliver fast".to_string())]
            .into_iter()
            .collect(),
        user_attributions: [(
            0usize,
            manox_harness::session_meta::UserAttributionMeta {
                author: "lead".into(),
                peer: false,
                display_text: None,
            },
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    manox_harness::session_meta::save(dir.path(), &session, &meta)
        .await
        .unwrap();

    clear_user_chrome(dir.path(), &session).await;

    let displays = load_registry_displays(dir.path(), &session).await;
    assert!(
        displays.is_empty(),
        "a compaction clears the stale display ordinals"
    );
    let attributions = load_user_attributions(dir.path(), &session).await;
    assert!(
        attributions.is_empty(),
        "a compaction clears the stale attributions"
    );
}

#[tokio::test]
async fn clear_user_chrome_is_noop_when_empty() {
    let dir = tempfile::tempdir().unwrap();
    let session = dir.path().join("sess-display-empty.jsonl");

    clear_user_chrome(dir.path(), &session).await;

    let displays = load_registry_displays(dir.path(), &session).await;
    assert!(displays.is_empty());
}

#[tokio::test]
async fn permission_mode_sidecar_tolerates_unknown_values() {
    let dir = tempfile::tempdir().unwrap();
    let session = dir.path().join("sess-2.jsonl");
    let meta = manox_harness::session_meta::SessionMeta {
        approval_mode: Some("yolo".to_string()),
        ..Default::default()
    };
    manox_harness::session_meta::save(dir.path(), &session, &meta)
        .await
        .unwrap();
    assert_eq!(
        load_approval_mode(dir.path(), &session).await,
        PermissionMode::WorkspaceWrite,
        "unknown persisted modes fall back to the bounded default"
    );
}

#[tokio::test]
async fn permission_mode_write_preserves_other_sidecar_fields() {
    let dir = tempfile::tempdir().unwrap();
    let session = dir.path().join("sess-3.jsonl");
    let meta = manox_harness::session_meta::SessionMeta {
        title: Some("my thread".to_string()),
        project: Some("/tmp/proj".to_string()),
        ..Default::default()
    };
    manox_harness::session_meta::save(dir.path(), &session, &meta)
        .await
        .unwrap();

    write_approval_mode_sidecar(dir.path(), &session, PermissionMode::DangerFullAccess)
        .await
        .unwrap();

    let loaded = manox_harness::session_meta::load(dir.path(), &session)
        .await
        .unwrap();
    assert_eq!(loaded.title.as_deref(), Some("my thread"));
    assert_eq!(loaded.project.as_deref(), Some("/tmp/proj"));
    assert_eq!(loaded.approval_mode.as_deref(), Some("danger-full-access"));
}
/// Build an idle `AgentSession` backed by a stream that answers every
/// request with a terminal assistant turn, so a `continue_` round driven by
/// the actor's continuation helper runs to settle in the test.
async fn steer_test_session(dir: &tempfile::TempDir) -> AgentSession {
    let cwd = dir.path().join("proj");
    std::fs::create_dir_all(&cwd).unwrap();
    let resolver: manox_harness::agent_loop::StreamResolver = Arc::new(|_m: &PiModel| {
        Ok(Arc::new(StaticStream) as Arc<dyn manox_harness::agent_loop::StreamFn>)
    });
    create_agent_session()
        .with_cwd(&cwd)
        .with_session_dir(dir.path().join("sessions"))
        .with_agent_dir(dir.path().join("agent"))
        .with_model_runtime(ModelRuntime::new(resolver))
        .with_model(test_model())
        .with_system_prompt("You are a test assistant.")
        .build()
        .await
        .unwrap()
}

/// Collect `(id, user)` for every `user` message row currently in the
/// session journal, so a test can assert the durable row identity.
async fn user_row_ids(session: &AgentSession) -> Vec<String> {
    let rows = session
        .journal_appender()
        .storage()
        .journal_range(0, u64::MAX)
        .await
        .unwrap();
    rows.iter()
        .filter_map(|r| match &r.entry {
            manox_harness::session::SessionTreeEntry::Message {
                id,
                message: AgentMessage::User { .. },
                ..
            } => Some(id.clone()),
            _ => None,
        })
        .collect()
}

/// S1 (dsh `wakeDriver` idle-steer) + S3 (stable row id): a `Steer`
/// arriving while the actor is idle starts its OWN run immediately, and
/// the injected `user` journal row is keyed by the client message id.
#[tokio::test]
async fn idle_steer_starts_its_own_run_and_lands_the_client_row_id() {
    // settle_run fires the detached plugin `Stop` hook through the global
    // runtime handle; init it hermetically like every settle-exercising test.
    crate::runtime::init_hermetic_for_test();
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("proj");
    std::fs::create_dir_all(&cwd).unwrap();
    let mut session = steer_test_session(&dir).await;
    let sessions_dir = dir.path().join("sessions");
    // Seed one assistant turn so the session is genuinely mid-conversation:
    // `continue_` refuses to run from an EMPTY transcript ("No messages to
    // continue from"), and the idle-steer arm is only reached while a turn
    // is in flight (a facade/actor race) — i.e. after at least one turn.
    session
        .append_message(AgentMessage::user("seed"))
        .await
        .unwrap();
    session
        .continue_()
        .await
        .expect("seed turn must stream under the test model");

    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<SessionCmd>();
    let (notice_tx, mut notice_rx) = mpsc::unbounded_channel::<BackendNotice>();
    let state = test_engine_state();
    let live = Arc::new(Mutex::new(LiveTranscript::default()));
    let mut run_steers = Vec::new();
    let mut shutdown_after_run = false;
    let mut pi_model = test_model();

    // The between-runs `Steer` arm: enqueue under the client id (S3/S4) and
    // remember the id for the settle confirmation.
    let client_id = "client-steer-777".to_string();
    session.handle().steer_with_id(
        steer_message(client_id.clone(), "hello there".into(), Vec::new()),
        client_id.clone(),
    );
    run_steers.push(client_id.clone());

    // S1: the actor resumes the idle steer through the shared helper.
    resume_steering_queue(
        &mut session,
        &mut cmd_rx,
        &mut run_steers,
        &mut shutdown_after_run,
        Arc::clone(&live),
        &state,
        &notice_tx,
        &mut pi_model,
        &sessions_dir,
        &cwd,
    )
    .await;

    // The queue drained (the run consumed the steer) and no shutdown.
    assert!(
        session.steering_messages().is_empty(),
        "S1: the resume must drain the steering queue"
    );
    assert!(!shutdown_after_run);
    assert!(
        !state.running.load(Ordering::Relaxed),
        "the run settled back to idle"
    );

    // TurnStarted fired (a run actually started), then Settled confirms the
    // steer on that run.
    let mut saw_turn_started = false;
    let mut settled: Option<(Vec<String>, Vec<String>, bool, bool)> = None;
    while let Ok(notice) = notice_rx.try_recv() {
        match notice {
            BackendNotice::Event(event) => {
                if matches!(*event, ThreadEvent::TurnStarted) {
                    saw_turn_started = true;
                }
            }
            BackendNotice::Settled {
                steered,
                stranded,
                cancelled,
                failed,
            } => settled = Some((steered, stranded, cancelled, failed)),
            _ => {}
        }
    }
    assert!(
        saw_turn_started,
        "S1: an idle steer must start its own run (TurnStarted)"
    );
    let (steered, stranded, cancelled, failed) = settled.expect("the resume must settle");
    assert_eq!(steered, vec![client_id.clone()]);
    assert!(stranded.is_empty());
    assert!(!cancelled && !failed);

    // S3: the durable `user` row carries the client message id.
    let ids = user_row_ids(&session).await;
    assert!(
        ids.iter().any(|id| id == &client_id),
        "S3: the injected user row id must equal the client message id; got {ids:?}"
    );
    let _ = cmd_tx; // keep the sender alive for the run
}

/// S2 (pi `agent-loop` / omp settle-drain): a steer still queued when a
/// run settles is drained by the same continuation helper, so it is never
/// silently deferred to an unrelated later turn.
#[tokio::test]
async fn settle_drains_a_steer_left_queued_at_run_end() {
    crate::runtime::init_hermetic_for_test();
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("proj");
    std::fs::create_dir_all(&cwd).unwrap();
    let mut session = steer_test_session(&dir).await;
    let sessions_dir = dir.path().join("sessions");
    // Seed one assistant turn so a later `continue_` can run: `continue_`
    // refuses to resume from an empty transcript.
    session
        .append_message(AgentMessage::user("seed"))
        .await
        .unwrap();
    session
        .continue_()
        .await
        .expect("seed turn must stream under the test model");
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<SessionCmd>();
    let (notice_tx, mut notice_rx) = mpsc::unbounded_channel::<BackendNotice>();
    let state = test_engine_state();
    let live = Arc::new(Mutex::new(LiveTranscript::default()));
    let mut run_steers = Vec::new();
    let mut shutdown_after_run = false;
    let mut pi_model = test_model();

    // A steer lands in the run's final moments: still in the queue after
    // the settle point.
    let late_id = "late-steer-1".to_string();
    session.handle().steer_with_id(
        steer_message(late_id.clone(), "one more thing".into(), Vec::new()),
        late_id.clone(),
    );
    run_steers.push(late_id.clone());
    assert!(!session.steering_messages().is_empty());

    // The post-settle drain (S2) is the same helper — it sees a non-empty
    // queue and runs it to empty.
    resume_steering_queue(
        &mut session,
        &mut cmd_rx,
        &mut run_steers,
        &mut shutdown_after_run,
        Arc::clone(&live),
        &state,
        &notice_tx,
        &mut pi_model,
        &sessions_dir,
        &cwd,
    )
    .await;

    assert!(
        session.steering_messages().is_empty(),
        "S2: the post-settle drain must empty the queue"
    );
    let ids = user_row_ids(&session).await;
    assert!(
        ids.iter().any(|id| id == &late_id),
        "S2: the late steer is realized by a run before it idles; got {ids:?}"
    );
    // Settled confirmation with the late steer.
    let mut confirmed = false;
    while let Ok(notice) = notice_rx.try_recv() {
        if let BackendNotice::Settled { steered, .. } = notice {
            confirmed |= steered.iter().any(|s| s == &late_id);
        }
    }
    assert!(confirmed, "S2: the drained steer confirms as injected");
    let _ = cmd_tx;
}

/// S4 (omp `#drainStrandedQueuedMessages` `#abortInProgress` guard): an
/// aborted run must RETRACT its not-yet-drained steer from the surviving
/// kernel queue and report it as stranded (not injected), so it is never
/// silently re-injected into the next run and a client retry does not
/// duplicate the message.
#[tokio::test]
async fn aborted_run_retracts_stranded_steers_from_the_kernel_queue() {
    // settle_run fires the Stop hook; init the hermetic runtime handle.
    crate::runtime::init_hermetic_for_test();
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("proj");
    std::fs::create_dir_all(&cwd).unwrap();
    let session = steer_test_session(&dir).await;
    let sessions_dir = dir.path().join("sessions");
    let state = test_engine_state();
    let (notice_tx, mut notice_rx) = mpsc::unbounded_channel::<BackendNotice>();

    // A steer queued while a run was in flight but NOT drained by it
    // (the loop parked the run and aborted before the injection boundary).
    let stranded_id = "stranded-steer".to_string();
    session.handle().steer_with_id(
        steer_message(
            stranded_id.clone(),
            "never reached the model".into(),
            Vec::new(),
        ),
        stranded_id.clone(),
    );
    assert!(
        !session.steering_messages().is_empty(),
        "setup: steer queued"
    );
    let mut run_steers = vec![stranded_id.clone()];

    // Abort path: settle_run with abort_requested must retract every still-
    // queued steer id and report the retracted ones as stranded.
    settle_run(
        &Ok(Vec::new()),
        /* abort_requested */ true,
        &session,
        &state,
        &sessions_dir,
        &cwd,
        &notice_tx,
        &mut run_steers,
    )
    .await;

    // The kernel queue is now empty: nothing can leak into the NEXT run.
    assert!(
        session.steering_messages().is_empty(),
        "S4: an aborted settle must retract stranded steers from the queue"
    );
    // run_steers was consumed.
    assert!(run_steers.is_empty(), "settle consumes the run's steer ids");
    // The retracted steer is reported stranded (NOT steered), so the client
    // sees a retryable Failed card rather than a phantom injection.
    let mut stranded_seen = false;
    while let Ok(notice) = notice_rx.try_recv() {
        if let BackendNotice::Settled {
            steered,
            stranded,
            cancelled,
            ..
        } = notice
        {
            assert!(cancelled);
            stranded_seen |= stranded.iter().any(|s| s == &stranded_id);
            assert!(
                !steered.iter().any(|s| s == &stranded_id),
                "a retracted steer must not be double-reported as injected"
            );
        }
    }
    assert!(
        stranded_seen,
        "S4: the aborted run reports the stranded steer as stranded"
    );
}

/// The last `turn_finish` journal row (settle delivery probe): the entry
/// id, parent id, and the §C.2 flags, as `settle_run` appended them.
async fn turn_finish_row(
    session: &AgentSession,
) -> (String, Option<String>, bool, bool, Vec<String>) {
    let rows = session
        .journal_appender()
        .storage()
        .journal_range(0, u64::MAX)
        .await
        .unwrap();
    let (_, entry) = rows
        .iter()
        .rev()
        .find_map(|r| match &r.entry {
            manox_harness::session::SessionTreeEntry::TurnFinish {
                id,
                parent_id,
                cancelled,
                failed,
                stranded_steer_ids,
                ..
            } => Some((
                r.seq,
                (
                    id.clone(),
                    parent_id.clone(),
                    *cancelled,
                    *failed,
                    stranded_steer_ids.clone(),
                ),
            )),
            _ => None,
        })
        .expect("settle must append a `turn_finish` journal row");
    // The row is the journal's tail: settle appends after every other
    // write of the run, so the seq stays chain-dense without renumbering.
    assert_eq!(
        rows.last().expect("journal non-empty").seq,
        rows.iter()
            .rev()
            .find_map(|r| match r.entry {
                manox_harness::session::SessionTreeEntry::TurnFinish { .. } => Some(r.seq),
                _ => None,
            })
            .expect("turn_finish row present"),
        "the `turn_finish` row must be the journal tail (dense seq)"
    );
    entry
}

/// Settle delivery (v2 wire): a normal settle appends a durable
/// `turn_finish` row — clean exit, no stranded steers, a fresh entry id
/// chained onto the session leaf.
#[tokio::test]
async fn settle_appends_a_turn_finish_row_on_normal_completion() {
    crate::runtime::init_hermetic_for_test();
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("proj");
    std::fs::create_dir_all(&cwd).unwrap();
    let session = steer_test_session(&dir).await;
    let sessions_dir = dir.path().join("sessions");
    let state = test_engine_state();
    let (notice_tx, _notice_rx) = mpsc::unbounded_channel::<BackendNotice>();
    // A steer drained by the run confirms as injected (not stranded).
    let mut run_steers = vec!["injected-steer".to_string()];
    let pre_leaf = session.journal_appender().leaf_id().await.unwrap();
    settle_run(
        &Ok(Vec::new()),
        /* abort_requested */ false,
        &session,
        &state,
        &sessions_dir,
        &cwd,
        &notice_tx,
        &mut run_steers,
    )
    .await;

    let (id, parent_id, cancelled, failed, stranded) = turn_finish_row(&session).await;
    assert!(!id.is_empty(), "the row carries a fresh entry id");
    // `append_typed` chains onto the session leaf and advances the
    // cursor: the row's parent is the pre-settle leaf (None on a fresh
    // journal) and the row itself is now the leaf.
    assert_eq!(parent_id, pre_leaf, "the row chains onto the leaf");
    assert_eq!(
        session
            .journal_appender()
            .leaf_id()
            .await
            .unwrap()
            .as_deref(),
        Some(id.as_str()),
        "the append moves the leaf cursor to the row"
    );
    assert!(!cancelled);
    assert!(!failed);
    assert!(stranded.is_empty(), "a clean settle strands nothing");
}

/// Settle delivery, abort path: the retracted steers of the
/// `Settled{stranded}` notice ride the same durable row, so a v2 client
/// retires its cards from the journal fold alone.
#[tokio::test]
async fn settle_appends_a_turn_finish_row_with_stranded_steers_on_abort() {
    crate::runtime::init_hermetic_for_test();
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("proj");
    std::fs::create_dir_all(&cwd).unwrap();
    let session = steer_test_session(&dir).await;
    let sessions_dir = dir.path().join("sessions");
    let state = test_engine_state();
    let (notice_tx, _notice_rx) = mpsc::unbounded_channel::<BackendNotice>();

    let stranded_id = "stranded-steer".to_string();
    session.handle().steer_with_id(
        steer_message(
            stranded_id.clone(),
            "never reached the model".into(),
            Vec::new(),
        ),
        stranded_id.clone(),
    );
    let mut run_steers = vec![stranded_id.clone()];
    settle_run(
        &Ok(Vec::new()),
        /* abort_requested */ true,
        &session,
        &state,
        &sessions_dir,
        &cwd,
        &notice_tx,
        &mut run_steers,
    )
    .await;

    let (_id, _parent, cancelled, failed, stranded) = turn_finish_row(&session).await;
    assert!(cancelled, "the abort verdict rides the row");
    assert!(!failed);
    assert_eq!(stranded, vec![stranded_id], "retracted steers are listed");
}

/// S4 continuation: the retraction is what stops the stranded text from
/// re-injecting on the next `continue_` (guard against the double-message
/// regression). A retried steer (fresh id) is the ONLY thing the next run
/// injects.
#[tokio::test]
async fn retried_steer_injects_once_after_abort() {
    crate::runtime::init_hermetic_for_test();
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("proj");
    std::fs::create_dir_all(&cwd).unwrap();
    let mut session = steer_test_session(&dir).await;
    let sessions_dir = dir.path().join("sessions");
    // Seed one assistant turn so a later `continue_` can run: `continue_`
    // refuses to resume from an empty transcript.
    session
        .append_message(AgentMessage::user("seed"))
        .await
        .unwrap();
    session
        .continue_()
        .await
        .expect("seed turn must stream under the test model");
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<SessionCmd>();
    let (notice_tx, _notice_rx) = mpsc::unbounded_channel::<BackendNotice>();
    let state = test_engine_state();
    let live = Arc::new(Mutex::new(LiveTranscript::default()));
    let mut run_steers = Vec::new();
    let mut shutdown_after_run = false;
    let mut pi_model = test_model();

    // Run 1 queues a steer, then aborts before draining it.
    let aborted_id = "aborted-then-retracted".to_string();
    session.handle().steer_with_id(
        steer_message(aborted_id.clone(), "original text".into(), Vec::new()),
        aborted_id.clone(),
    );
    settle_run(
        &Ok(Vec::new()),
        /* abort_requested */ true,
        &session,
        &state,
        &sessions_dir,
        &cwd,
        &notice_tx,
        &mut vec![aborted_id.clone()],
    )
    .await;
    // The retraction left the queue empty.
    assert!(session.steering_messages().is_empty());

    // The user retries with a fresh id. Only that new steer exists.
    let retry_id = "retry-fresh".to_string();
    session.handle().steer_with_id(
        steer_message(retry_id.clone(), "original text".into(), Vec::new()),
        retry_id.clone(),
    );
    resume_steering_queue(
        &mut session,
        &mut cmd_rx,
        &mut run_steers,
        &mut shutdown_after_run,
        Arc::clone(&live),
        &state,
        &notice_tx,
        &mut pi_model,
        &sessions_dir,
        &cwd,
    )
    .await;

    let ids = user_row_ids(&session).await;
    assert!(
        ids.contains(&retry_id),
        "the retried steer injects under its own id; got {ids:?}"
    );
    assert!(
        !ids.contains(&aborted_id),
        "S4: the retracted steer must NOT re-inject — a retry yields one message, not two"
    );
    let _ = cmd_tx;
}

#[test]
fn steer_message_carries_images_behind_text() {
    let msg = steer_message(
        "client-minted-id".to_string(),
        "look at this".to_string(),
        vec![manox_harness::types::ContentBlock::Image {
            data: "aW1hZ2U=".to_string(),
            mime_type: "image/png".to_string(),
        }],
    );
    let manox_harness::types::AgentMessage::User { content, id, .. } = &msg else {
        panic!("steer message must be a user message");
    };
    // S3: the client's steer message id rides the message so the injected
    // `user` journal row lands under it.
    assert_eq!(id.as_deref(), Some("client-minted-id"));
    assert_eq!(content.len(), 2, "text first, then the image block");
    assert!(matches!(
        &content[0],
        manox_harness::types::ContentBlock::Text { text, .. } if text == "look at this"
    ));
    assert!(matches!(
        &content[1],
        manox_harness::types::ContentBlock::Image { mime_type, .. } if mime_type == "image/png"
    ));
}

#[test]
fn bash_output_update_still_maps_to_tool_output() {
    let events = adapt::agent_event_to_thread_events(
        &manox_harness::types::AgentEvent::ToolExecutionUpdate {
            tool_call_id: "call-3".into(),
            tool_name: "Bash".into(),
            arguments: serde_json::json!({}),
            partial_result: serde_json::json!({ "output": "line one" }),
        },
    );
    assert_eq!(events.len(), 1);
    assert!(matches!(
        &events[0],
        crate::thread::ThreadEvent::ToolOutput { chunk, .. } if chunk == "line one"
    ));
}
#[test]
fn adapt_strips_proposed_plan_blocks_from_assistant_text() {
    let plan = "## Steps\n- do the thing";
    let messages = vec![manox_harness::types::AgentMessage::Assistant {
        content: vec![manox_harness::types::ContentBlock::Text {
            text: format!(
                "Here is my plan.\n\n<proposed_plan>\n{plan}\n</proposed_plan>\n\nShall we?"
            ),
            signature: None,
        }],
        model: "test".into(),
        provider: "test".into(),
        api: "test".into(),
        response_model: None,
        response_id: None,
        diagnostics: None,
        stop_reason: Some(manox_harness::types::StopReason::Stop),
        raw_stop_reason: None,
        usage: Box::new(manox_harness::types::Usage::default()),
        error_message: None,
        timestamp: chrono::Utc::now(),
    }];
    let mapped = adapt::harness_messages_to_messages(&messages);
    assert_eq!(mapped.len(), 1);
    let text = mapped[0]
        .content
        .iter()
        .find_map(|c| match c {
            crate::language_model::MessageContent::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .unwrap();
    assert!(
        !text.contains("<proposed_plan>"),
        "plan block must not render"
    );
    assert!(text.contains("Here is my plan."));
    assert!(text.contains("Shall we?"));
}

/// L3 mechanical verification: every ThreadEvent the tap maps must
/// have its (kind, payload) construct a typed journal entry — the
/// mapping and the vocabulary never drift apart.
#[test]
fn durable_journal_mapping_round_trips_every_journaled_event() {
    use crate::thread::{SubagentChildEvent, ThreadEvent, ToolCallStatus};
    let events: Vec<ThreadEvent> = vec![
        ThreadEvent::TurnStarted,
        ThreadEvent::TurnFinished {
            cancelled: false,
            failed: true,
            stranded_steer_ids: vec!["s-1".into()],
        },
        ThreadEvent::Stop(crate::language_model::StopReason::MaxTokens),
        ThreadEvent::Retry {
            attempt: 2,
            max_attempts: 5,
            delay_secs: 3,
            reason: "rate limited".into(),
            detail: Some("429".into()),
        },
        ThreadEvent::Error(anyhow::anyhow!("provider exploded")),
        ThreadEvent::AgentText("delta".into()),
        ThreadEvent::AgentThinking("think".into()),
        ThreadEvent::ToolCall {
            id: "tc-1".into(),
            name: "Bash".into(),
            title: "run ls".into(),
            status: ToolCallStatus::Running,
            input: Some(serde_json::json!({"command": "ls"})),
        },
        ThreadEvent::ToolResult {
            id: "tc-1".into(),
            output: "a b".into(),
            is_error: false,
        },
        ThreadEvent::ToolOutput {
            id: "tc-1".into(),
            chunk: "a".into(),
        },
        ThreadEvent::SubagentStarted {
            id: "sub-1".into(),
            subagent_type: "explore".into(),
            description: "scout".into(),
            child: crate::thread::ThreadId("child-1".into()),
        },
        ThreadEvent::SubagentProgress {
            id: "sub-1".into(),
            subagent_type: "explore".into(),
            tool_uses: 4,
            token_usage: Default::default(),
            latest_activity: Some("reading".into()),
            status: ToolCallStatus::Running,
            health: None,
        },
        ThreadEvent::SubagentChild {
            id: "sub-1".into(),
            child: SubagentChildEvent::Text("hi".into()),
        },
        ThreadEvent::PermissionModeChanged {
            mode: manox_harness::sandbox::PermissionMode::default(),
        },
        ThreadEvent::PlanModeChanged { enabled: true },
        ThreadEvent::PlanUpdated {
            snapshot: crate::plan::PlanSnapshot {
                explanation: None,
                steps: vec![],
            },
        },
        ThreadEvent::PlanReady {
            plan_file: "/plans/demo-plan.md".into(),
            title: "Demo plan".into(),
            content: "# Demo\n\n- step".into(),
        },
        ThreadEvent::GoalChanged { goal: None },
        ThreadEvent::TitleChanged { title: "t".into() },
        ThreadEvent::BrowserSuitesChanged {
            suites: vec![crate::engine::BrowserSuite::ChromeUse],
        },
        // BackgroundTaskUpdated omitted: TaskSnapshot has no cheap test
        // constructor; its mapping is a mechanical to_value into a
        // JsonValue entry field covered by from_kind_payload tests.
        ThreadEvent::ToolCallAuthorization {
            id: "auth-1".into(),
            tool_name: "Bash".into(),
            summary: "run ls".into(),
            input: serde_json::json!({"command": "ls"}),
        },
        ThreadEvent::CompactionStarted { tokens_before: 42 },
        ThreadEvent::TokenUsageUpdated(Default::default()),
        ThreadEvent::PrefixStability {
            stability_pct: 90,
            system_changed: false,
            tools_changed: false,
        },
        ThreadEvent::CacheInvalidation {
            reprocessed_tokens: 10,
        },
        ThreadEvent::SideCallMetricsUpdated(vec![]),
        ThreadEvent::MainCallMetricsUpdated(Default::default()),
    ];
    for event in &events {
        let (kind, payload) = durable_journal_payload(event)
            .unwrap_or_else(|| panic!("{event:?} must map to a journal kind"));
        let entry = manox_harness::session::SessionTreeEntry::from_kind_payload(
            &kind,
            "e-test".into(),
            None,
            chrono::Utc::now(),
            payload,
        )
        .unwrap_or_else(|err| panic!("kind {kind} payload must construct: {err}"));
        assert_eq!(entry.id(), "e-test");
    }
    // Events whose persistence/snapshot semantics belong to their
    // own flows must not double-enter the journal.
    let excluded = vec![
        ThreadEvent::ModelChanged {
            from: None,
            to: "m".into(),
        },
        ThreadEvent::ReasoningEffortChanged {
            effort: crate::language_model::ReasoningEffort::High,
        },
        ThreadEvent::CwdChanged { path: "/p".into() },
        ThreadEvent::HistoryProgress,
        ThreadEvent::HistoryRestored,
    ];
    for event in &excluded {
        assert!(
            durable_journal_payload(event).is_none(),
            "{event:?} must not journal (owned by another flow)"
        );
    }
}

/// The journal relay maps storage appends (and Lagged) into the thread
/// feed in order — the §C.3 host read face T4's follow streams ride.
#[tokio::test]
async fn journal_relay_feeds_storage_appends_in_seq_order() {
    crate::runtime::init_hermetic_for_test();
    let dir = tempfile::tempdir().unwrap();
    let storage = manox_harness::session::jsonl::JsonlSessionStorage::create(
        &dir.path().join("s.jsonl"),
        manox_harness::session::jsonl::JsonlSessionMetadata {
            id: "s1".into(),
            cwd: "/t".into(),
            created_at: chrono::Utc::now(),
            parent_session_path: None,
            metadata: None,
        },
    )
    .await
    .unwrap();
    let state = test_engine_state();
    spawn_journal_relay_rx(storage.subscribe_journal(), state.journal_tx.clone());
    let mut feed = state.journal_tx.subscribe();

    use manox_harness::session::{SessionStorage, SessionTreeEntry};
    for i in 0..3 {
        let parent = if i == 0 {
            None
        } else {
            Some(format!("e{}", i - 1))
        };
        let parent = parent.as_deref();
        storage
            .append_entry(&SessionTreeEntry::TurnStart {
                id: format!("e{i}"),
                parent_id: parent.map(str::to_string),
                timestamp: chrono::Utc::now(),
            })
            .await
            .unwrap();
    }
    for want in 0..3u64 {
        match feed.recv().await.expect("feed event") {
            JournalFeed::Event(ev) => assert_eq!(ev.seq, want),
            JournalFeed::Lagged(n) => panic!("unexpected lag {n}"),
        }
    }
}

/// F7 guard: the ask path journals as `question`, everything else as
/// `approval`. The vocabulary 1:1 tests only prove the entry exists; this
/// pins the split itself.
#[test]
fn ask_authorizations_journal_as_questions_not_approvals() {
    let kind_of = |tool_name: &str| {
        durable_journal_payload(&ThreadEvent::ToolCallAuthorization {
            id: "auth-1".into(),
            tool_name: tool_name.into(),
            summary: "card".into(),
            input: serde_json::json!({}),
        })
        .expect("authorizations are journaled")
        .0
    };
    assert_eq!(kind_of(crate::tools::ASK_USER_QUESTION), "question");
    assert_eq!(kind_of("Bash"), "approval");
}

/// F6 guard: the turn-boundary commit applies a pending selection, consumes
/// it, announces the switch, and stays silent for a converged selection.
#[tokio::test]
async fn boundary_commit_applies_pending_plan_mode_and_converges_silently() {
    crate::runtime::init_hermetic_for_test();
    let dir = tempfile::tempdir().unwrap();
    let session = steer_test_session(&dir).await;
    let (notice_tx, mut notice_rx) = mpsc::unbounded_channel();
    let state = test_engine_state();

    state.plan.set_requested(Some(true));
    assert!(
        !state.plan.enabled(),
        "the selection waits for the boundary"
    );
    commit_requested_plan_mode(session.path(), dir.path(), &state, &notice_tx).await;
    assert!(state.plan.enabled(), "the boundary applies the selection");
    assert_eq!(state.plan.requested(), None, "and consumes it");
    let mut announced = false;
    while let Ok(notice) = notice_rx.try_recv() {
        if let BackendNotice::Event(event) = notice
            && matches!(*event, ThreadEvent::PlanModeChanged { enabled: true })
        {
            announced = true;
        }
    }
    assert!(announced, "the commit announces the mode change");

    // A selection equal to the committed state is a no-op intent.
    state.plan.set_requested(Some(true));
    commit_requested_plan_mode(session.path(), dir.path(), &state, &notice_tx).await;
    assert_eq!(state.plan.requested(), None);
    assert!(
        notice_rx.try_recv().is_err(),
        "a converged selection must not announce anything"
    );
}

fn test_engine_state() -> Arc<EngineState> {
    let cwd = std::env::temp_dir();
    let (notice_tx, _notice_rx) = mpsc::unbounded_channel();
    let model_slot = Arc::new(Mutex::new(None));
    let gate = Arc::new(ApprovalGate::new(
        notice_tx.clone(),
        Arc::clone(&model_slot),
    ));
    let question_gate = Arc::new(crate::questions::UserQuestionGate::new(notice_tx.clone()));
    Arc::new(EngineState {
        running: AtomicBool::new(false),
        session_start_fired: AtomicBool::new(false),
        history: Mutex::new(Vec::new()),
        notes: Mutex::new(Vec::new()),
        notes_gen: AtomicU64::new(0),
        pending_ui_notes: Mutex::new(Vec::new()),
        pending_journal: Mutex::new(Vec::new()),
        journal_tx: tokio::sync::broadcast::channel(JOURNAL_FEED_CAPACITY).0,
        request_usage: Mutex::new(HashMap::new()),
        per_model_last_usage: Mutex::new(HashMap::new()),
        cumulative: Mutex::new(TokenUsage::default()),
        per_model: Mutex::new(HashMap::new()),
        cumulative_cost: Mutex::new(0.0),
        per_model_cost: Mutex::new(HashMap::new()),
        model: model_slot,
        sessions: Mutex::new(Vec::new()),
        active_path: Mutex::new(None),
        pending_browser_suite: Mutex::new(None),
        pending_session_cmds: Mutex::new(Vec::new()),
        current_appender: Mutex::new(None),
        current_resources: Mutex::new(None),
        gate,
        question_gate,
        plan: crate::plan_mode::PlanSessionState::new(),
        plan_review_request_id: Mutex::new(None),
        goal_bridge: None,
        goal_continuation_reserved: AtomicBool::new(false),
        goal_continuation_round: Mutex::new(None),
        granted_roots: crate::granted_roots::GrantedRoots::new(cwd.clone()),
        last_cwd_note: Mutex::new(None),
    })
}

fn partial_assistant(text: &str) -> AgentMessage {
    AgentMessage::Assistant {
        content: vec![ContentBlock::Text {
            text: text.to_string(),
            signature: None,
        }],
        model: "test".into(),
        provider: "test".into(),
        api: "anthropic".into(),
        response_model: None,
        response_id: None,
        diagnostics: None,
        stop_reason: None,
        raw_stop_reason: None,
        usage: Box::new(manox_harness::types::Usage::default()),
        error_message: None,
        timestamp: chrono::Utc::now(),
    }
}

/// The live-history mirror serves completed messages plus the streaming
/// partial (kernel `streaming_message` parity), and reports change only
/// when the snapshot actually moved (the idle-tick guard).
#[test]
fn sync_live_history_mirrors_completed_plus_streaming_and_reports_change() {
    let state = test_engine_state();
    let live = Arc::new(Mutex::new(LiveTranscript::default()));
    live.lock().unwrap().messages.push(AgentMessage::user("hi"));
    live.lock().unwrap().streaming = Some(partial_assistant("part"));

    assert!(sync_live_history(&live, &state));
    {
        let history = state.history.lock().unwrap();
        assert_eq!(history.len(), 2);
        assert!(matches!(
            &history[0],
            HistoryEntry::Message(m)
                if matches!(&m.content[0], crate::language_model::MessageContent::Text(t) if t == "hi")
        ));
        assert!(matches!(
            &history[1],
            HistoryEntry::Message(m)
                if matches!(&m.content[0], crate::language_model::MessageContent::Text(t) if t == "part")
        ));
    }

    // Identical snapshot: no change (a run parked on an approval verdict
    // streams nothing, so the facade notice is skipped).
    assert!(!sync_live_history(&live, &state));

    // The streaming partial growing re-syncs.
    live.lock().unwrap().streaming = Some(partial_assistant("partial-answer"));
    assert!(sync_live_history(&live, &state));
    let history = state.history.lock().unwrap();
    assert!(matches!(
        &history[1],
        HistoryEntry::Message(m)
            if matches!(&m.content[0], crate::language_model::MessageContent::Text(t) if t == "partial-answer")
    ));
}

/// A stream that issues one tool call then stops, recording the model of
/// every provider request and parking on barriers so the test can
/// interleave a mid-run model switch between the two turns.
struct MidRunModelStream {
    calls: std::sync::atomic::AtomicUsize,
    seen: Arc<std::sync::Mutex<Vec<String>>>,
    turn1_started: Arc<tokio::sync::Notify>,
    release1: Arc<tokio::sync::Notify>,
    turn2_started: Arc<tokio::sync::Notify>,
    release2: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl manox_harness::agent_loop::StreamFn for MidRunModelStream {
    async fn stream(
        &self,
        context: &manox_harness::types::AgentContext,
        _signal: tokio_util::sync::CancellationToken,
        _event_tx: tokio::sync::mpsc::Sender<manox_harness::types::AgentEvent>,
    ) -> Result<manox_harness::types::AgentMessage, anyhow::Error> {
        let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.seen.lock().unwrap().push(context.model.id.clone());
        if n == 0 {
            self.turn1_started.notify_waiters();
            self.release1.notified().await;
            Ok(AgentMessage::Assistant {
                content: vec![ContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "echo".into(),
                    input: serde_json::json!({"message": "hi"}),
                    thought_signature: None,
                }],
                model: context.model.id.clone(),
                provider: context.model.provider.clone(),
                api: context.model.api.clone(),
                response_model: None,
                response_id: None,
                diagnostics: None,
                raw_stop_reason: None,
                stop_reason: Some(manox_harness::types::StopReason::ToolUse),
                usage: Box::new(manox_harness::types::Usage {
                    input_tokens: 100,
                    output_tokens: 10,
                    ..Default::default()
                }),
                error_message: None,
                timestamp: chrono::Utc::now(),
            })
        } else {
            self.turn2_started.notify_waiters();
            self.release2.notified().await;
            Ok(AgentMessage::Assistant {
                content: vec![ContentBlock::Text {
                    text: "done".into(),
                    signature: None,
                }],
                model: context.model.id.clone(),
                provider: context.model.provider.clone(),
                api: context.model.api.clone(),
                response_model: None,
                response_id: None,
                diagnostics: None,
                raw_stop_reason: None,
                stop_reason: Some(manox_harness::types::StopReason::Stop),
                usage: Box::new(manox_harness::types::Usage {
                    input_tokens: 100,
                    output_tokens: 10,
                    ..Default::default()
                }),
                error_message: None,
                timestamp: chrono::Utc::now(),
            })
        }
    }
}

/// The `echo` tool the mid-run stream calls, so the run spans two turns.
struct EchoTool;

#[async_trait::async_trait]
impl manox_harness::tool::AgentTool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "Echoes the input"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!(
            {"type": "object", "properties": {"message": {"type": "string"}}}
        )
    }
    async fn execute(
        &self,
        _id: &str,
        params: serde_json::Value,
        _signal: tokio_util::sync::CancellationToken,
        _ctx: &dyn manox_harness::tool::ToolContext,
    ) -> Result<manox_harness::tool::AgentToolResult, manox_harness::tool::ToolError> {
        Ok(manox_harness::tool::AgentToolResult::text(
            params["message"].as_str().unwrap_or("no message"),
        ))
    }
}

fn test_model_switched() -> PiModel {
    PiModel {
        provider: "test".into(),
        api: "test".into(),
        id: "new".into(),
        context_window: 100_000,
        max_tokens: 8_192,
        thinking: manox_harness::types::ThinkingKind::None,
        metadata: Default::default(),
    }
}

/// H3 guard: the plan-mode boundary commit sits at the shared run start,
/// so a run that does not come through the actor's Prompt arm (a goal
/// round, a drained steer, a monitor wake-up — modelled here by calling
/// `drive_run` directly) still commits a pending selection before the run
/// future is polled. Reverting the commit to the Prompt arm alone makes
/// this test fail with `enabled() == false`.
#[tokio::test]
async fn drive_run_commits_a_pending_plan_mode_selection_before_the_run() {
    crate::runtime::init_hermetic_for_test();
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("proj");
    std::fs::create_dir_all(&cwd).unwrap();
    let resolver: manox_harness::agent_loop::StreamResolver = Arc::new(|_m: &PiModel| {
        Ok(Arc::new(StaticStream) as Arc<dyn manox_harness::agent_loop::StreamFn>)
    });
    let mut session = create_agent_session()
        .with_cwd(&cwd)
        .with_session_dir(dir.path().join("sessions"))
        .with_agent_dir(dir.path().join("agent"))
        .with_model_runtime(ModelRuntime::new(resolver))
        .with_model(test_model())
        .with_system_prompt("You are a test assistant.")
        .build()
        .await
        .unwrap();

    let (_cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<SessionCmd>();
    let (notice_tx, mut notice_rx) = mpsc::unbounded_channel::<BackendNotice>();
    let state = test_engine_state();
    let live = Arc::new(Mutex::new(LiveTranscript::default()));
    let mut run_steers = Vec::new();
    let mut shutdown_after_run = false;
    let mut pi_model = test_model();
    let handle = session.handle();
    let sessions_path = dir.path().join("sessions");
    let active_session_path = session.path().clone();
    let journal_appender = session.journal_appender();

    // A selection is outstanding when the run starts.
    state.plan.set_requested(Some(true));
    assert!(!state.plan.enabled());

    let run = drive_run(
        session.prompt("first turn"),
        &handle,
        &mut cmd_rx,
        &mut run_steers,
        &mut shutdown_after_run,
        live,
        &state,
        &notice_tx,
        &mut pi_model,
        &sessions_path,
        &active_session_path,
        &journal_appender,
    );
    let (_result, _aborted) = run.await;
    assert!(
        state.plan.enabled(),
        "the run start must commit the pending selection"
    );
    assert_eq!(state.plan.requested(), None, "and consume it");
    let mut announced = false;
    while let Ok(notice) = notice_rx.try_recv() {
        if let BackendNotice::Event(event) = notice
            && matches!(*event, ThreadEvent::PlanModeChanged { enabled: true })
        {
            announced = true;
        }
    }
    assert!(announced, "the commit announces the switch");
}

fn test_model() -> PiModel {
    PiModel {
        provider: "test".into(),
        api: "test".into(),
        id: "test".into(),
        context_window: 100_000,
        max_tokens: 8_192,
        thinking: manox_harness::types::ThinkingKind::None,
        metadata: Default::default(),
    }
}

/// A model switch arriving while a turn is in flight must reach the next
/// provider request: `drive_run` routes `SetModel` through the harness
/// handle (the turn runtime) instead of dropping it, so the turn after
/// the switch streams under the new model and the session attributes its
/// usage to it. Regression for the mid-conversation switch that showed
/// the new model in the UI while requests still ran the old one.
#[tokio::test]
async fn mid_run_model_switch_applies_to_next_turn_and_stats() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("proj");
    tokio::fs::create_dir_all(&cwd).await.unwrap();

    let stream = Arc::new(MidRunModelStream {
        calls: std::sync::atomic::AtomicUsize::new(0),
        seen: Arc::new(std::sync::Mutex::new(Vec::new())),
        turn1_started: Arc::new(tokio::sync::Notify::new()),
        release1: Arc::new(tokio::sync::Notify::new()),
        turn2_started: Arc::new(tokio::sync::Notify::new()),
        release2: Arc::new(tokio::sync::Notify::new()),
    });
    let stream_for_resolver = Arc::clone(&stream);
    let resolver: manox_harness::agent_loop::StreamResolver = Arc::new(move |_m: &PiModel| {
        Ok(Arc::clone(&stream_for_resolver) as Arc<dyn manox_harness::agent_loop::StreamFn>)
    });
    let runtime = ModelRuntime::new(resolver);

    let mut session = create_agent_session()
        .with_cwd(&cwd)
        .with_session_dir(dir.path().join("sessions"))
        .with_agent_dir(dir.path().join("agent"))
        .with_model_runtime(runtime)
        .with_model(test_model())
        .with_tools(vec![
            Arc::new(EchoTool) as Arc<dyn manox_harness::tool::AgentTool>
        ])
        .with_system_prompt("You are a test assistant.")
        .build()
        .await
        .unwrap();

    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<SessionCmd>();
    let (notice_tx, _notice_rx) = mpsc::unbounded_channel::<BackendNotice>();
    let state = test_engine_state();
    let live = Arc::new(Mutex::new(LiveTranscript::default()));
    let mut run_steers = Vec::new();
    let mut shutdown_after_run = false;
    let mut pi_model = test_model();

    let handle = session.handle();
    let sessions_path = dir.path().join("sessions");
    let active_session_path = session.path().clone();
    let journal_appender = session.journal_appender();
    let run = drive_run(
        session.prompt("first turn"),
        &handle,
        &mut cmd_rx,
        &mut run_steers,
        &mut shutdown_after_run,
        live,
        &state,
        &notice_tx,
        &mut pi_model,
        &sessions_path,
        &active_session_path,
        &journal_appender,
    );
    let new_model = test_model_switched();

    let ((result, _aborted), ()) = tokio::join!(run, async {
        // Turn 1 in flight; switch the model before it resumes. While
        // the run parks on the barrier, drive_run's select polls the
        // command channel, so the switch lands before turn 1 returns.
        stream.turn1_started.notified().await;
        cmd_tx
            .send(SessionCmd::SetModel(new_model.clone()))
            .unwrap();
        // A re-pick of the model already in play changes nothing: the
        // second command must not append another entry.
        cmd_tx
            .send(SessionCmd::SetModel(new_model.clone()))
            .unwrap();
        for _ in 0..10_000 {
            if state.model.lock().unwrap().as_ref().map(|m| m.id.as_str()) == Some("new") {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            state.model.lock().unwrap().as_ref().map(|m| m.id.as_str()),
            Some("new"),
            "drive_run must apply the mid-run SetModel while the turn is in flight"
        );
        stream.release1.notify_waiters();
        // Turn 2 streams under the switched model; release it.
        stream.turn2_started.notified().await;
        stream.release2.notify_waiters();
    });
    result.unwrap();

    assert_eq!(
        *stream.seen.lock().unwrap(),
        vec!["test".to_string(), "new".to_string()],
        "the turn after the mid-run switch must stream under the new model"
    );
    assert_eq!(
        pi_model.id, "new",
        "the actor's working model follows the switch"
    );
    // The session attributes the switched turn's usage to the new model:
    // the per-model breakdown now carries both identities.
    let stats = session.session_stats().await.unwrap();
    assert!(
        stats.per_model.iter().any(|e| e.key == "test/new"),
        "switched-turn usage must enter the per-model stats: {:?}",
        stats.per_model
    );
    // The switch persisted as exactly one model_change entry, so a reload
    // attributes the same history the same way.
    let jsonl = tokio::fs::read_to_string(session.path()).await.unwrap();
    assert_eq!(
        jsonl.matches("\"modelId\":\"new\"").count(),
        1,
        "the mid-run switch must persist one model_change entry for the new model, even \
             when the same model is picked twice: {jsonl}"
    );
}

/// A stream that parks once mid-run (barrier), then answers with text —
/// lets a test interleave a mid-run journal append while the turn is
/// provably in flight.
struct ParkOnceStream {
    started: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl manox_harness::agent_loop::StreamFn for ParkOnceStream {
    async fn stream(
        &self,
        context: &manox_harness::types::AgentContext,
        _signal: tokio_util::sync::CancellationToken,
        _event_tx: tokio::sync::mpsc::Sender<manox_harness::types::AgentEvent>,
    ) -> Result<manox_harness::types::AgentMessage, anyhow::Error> {
        self.started.notify_waiters();
        self.release.notified().await;
        Ok(AgentMessage::Assistant {
            content: vec![ContentBlock::Text {
                text: "done".into(),
                signature: None,
            }],
            model: context.model.id.clone(),
            provider: context.model.provider.clone(),
            api: context.model.api.clone(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            raw_stop_reason: None,
            stop_reason: Some(manox_harness::types::StopReason::Stop),
            usage: Box::new(manox_harness::types::Usage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            }),
            error_message: None,
            timestamp: chrono::Utc::now(),
        })
    }
}

/// A scripted provider that walks `rounds` rounds of TWO PARALLEL
/// read-only tool calls before answering with plain text — the user's
/// round-11 stall shape (three completed Grep rounds, then the fourth
/// round's toolResult rows never landed and the turn spun forever).
struct ToolRoundsStream {
    rounds: usize,
    call: std::sync::atomic::AtomicUsize,
}

fn tool_rounds_assistant(
    context: &manox_harness::types::AgentContext,
    round: usize,
) -> AgentMessage {
    let tool_use = |suffix: char| ContentBlock::ToolUse {
        id: format!("r{round}-{suffix}"),
        name: "Grep".into(),
        input: serde_json::json!({ "pattern": format!("needle{round}{suffix}") }),
        thought_signature: None,
    };
    AgentMessage::Assistant {
        content: vec![tool_use('a'), tool_use('b')],
        model: context.model.id.clone(),
        provider: context.model.provider.clone(),
        api: context.model.api.clone(),
        response_model: None,
        response_id: None,
        diagnostics: None,
        raw_stop_reason: None,
        stop_reason: Some(manox_harness::types::StopReason::ToolUse),
        usage: Box::new(manox_harness::types::Usage {
            input_tokens: 10,
            output_tokens: 5,
            ..Default::default()
        }),
        error_message: None,
        timestamp: chrono::Utc::now(),
    }
}

fn text_assistant(context: &manox_harness::types::AgentContext, text: &str) -> AgentMessage {
    AgentMessage::Assistant {
        content: vec![ContentBlock::Text {
            text: text.into(),
            signature: None,
        }],
        model: context.model.id.clone(),
        provider: context.model.provider.clone(),
        api: context.model.api.clone(),
        response_model: None,
        response_id: None,
        diagnostics: None,
        raw_stop_reason: None,
        stop_reason: Some(manox_harness::types::StopReason::Stop),
        usage: Box::new(manox_harness::types::Usage {
            input_tokens: 10,
            output_tokens: 5,
            ..Default::default()
        }),
        error_message: None,
        timestamp: chrono::Utc::now(),
    }
}

#[async_trait::async_trait]
impl manox_harness::agent_loop::StreamFn for ToolRoundsStream {
    async fn stream(
        &self,
        context: &manox_harness::types::AgentContext,
        _signal: tokio_util::sync::CancellationToken,
        _event_tx: tokio::sync::mpsc::Sender<manox_harness::types::AgentEvent>,
    ) -> Result<AgentMessage, anyhow::Error> {
        let n = self.call.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if n < self.rounds {
            Ok(tool_rounds_assistant(context, n))
        } else {
            Ok(text_assistant(context, "done"))
        }
    }
}

/// Round-11 regression: a multi-round turn of PARALLEL read-only tool
/// calls must land EVERY round's durable rows — each round's two
/// `tool_call` completions, their `tool_result` tap rows, AND the
/// persisted `ToolResult` messages (the persistence middleware). The
/// user's live stall: round 3's second Grep appended its success
/// `tool_call` row and then nothing — no `tool_result` row, no ToolResult
/// messages, turn frozen mid-flight forever. This test drives the real
/// three-writer stack (persistence middleware + notice tap → live
/// AppendJournal through drive_run's select) under a bounded timeout: a
/// stall fails the timeout with a journal dump instead of hanging.
#[tokio::test]
async fn parallel_tool_rounds_land_every_result() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("proj");
    tokio::fs::create_dir_all(&cwd).await.unwrap();

    let stream = Arc::new(ToolRoundsStream {
        rounds: 4,
        call: std::sync::atomic::AtomicUsize::new(0),
    });
    let stream_for_resolver = Arc::clone(&stream);
    let resolver: manox_harness::agent_loop::StreamResolver = Arc::new(move |_m: &PiModel| {
        Ok(Arc::clone(&stream_for_resolver) as Arc<dyn manox_harness::agent_loop::StreamFn>)
    });

    let mut session = create_agent_session()
        .with_cwd(&cwd)
        .with_session_dir(dir.path().join("sessions"))
        .with_agent_dir(dir.path().join("agent"))
        .with_model_runtime(ModelRuntime::new(resolver))
        .with_model(test_model())
        .with_system_prompt("You are a test assistant.")
        .build()
        .await
        .unwrap();

    // The engine's notice wiring, replicated: session events → ThreadEvent
    // notices → the tap queues durable AppendJournal cmds onto the actor
    // channel; drive_run services them live (mid-run arm).
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<SessionCmd>();
    let (tap_notice_tx, tap_notice_rx) = mpsc::unbounded_channel::<BackendNotice>();
    let (facade_tx, _facade_rx) = mpsc::unbounded_channel::<BackendNotice>();
    let _listener_sub = session.subscribe(Arc::new(move |event, _cancel| {
        let tx = tap_notice_tx.clone();
        Box::pin(async move {
            for te in crate::engine::adapt::agent_event_to_thread_events(&event) {
                let _ = tx.send(BackendNotice::Event(Box::new(te)));
            }
        })
    }));
    let tap_cmd_tx = cmd_tx.clone();
    let _tap = tokio::spawn(async move {
        let mut rx = tap_notice_rx;
        while let Some(notice) = rx.recv().await {
            if let BackendNotice::Event(event) = &notice
                && let Some((kind, payload)) = durable_journal_payload(event)
            {
                let _ = tap_cmd_tx.send(SessionCmd::AppendJournal { kind, payload });
            }
        }
    });

    let state = test_engine_state();
    let live = Arc::new(Mutex::new(LiveTranscript::default()));
    let mut run_steers = Vec::new();
    let mut shutdown_after_run = false;
    let mut pi_model = test_model();
    let handle = session.handle();
    let sessions_path = dir.path().join("sessions");
    let active_session_path = session.path().clone();
    let journal_appender = session.journal_appender();

    let driven = drive_run(
        session.prompt("four rounds of parallel greps"),
        &handle,
        &mut cmd_rx,
        &mut run_steers,
        &mut shutdown_after_run,
        live,
        &state,
        &facade_tx,
        &mut pi_model,
        &sessions_path,
        &active_session_path,
        &journal_appender,
    );
    let appender_for_dump = std::sync::Arc::clone(&journal_appender);
    // Production pressure the basic flow lacks: the foreground leaf fires
    // GetConversationInfo on every committed-message change, and each one
    // routes through SessionCmd::JournalSnapshot into drive_run's LIVE
    // arm (round-9). A concurrent snapshot storm rides the whole turn.
    let snapshot_cmd_tx = cmd_tx.clone();
    let snapshot_storm = tokio::spawn(async move {
        for _ in 0..2000u32 {
            let (tx, rx) = tokio::sync::oneshot::channel::<JournalSnapshotData>();
            if snapshot_cmd_tx
                .send(SessionCmd::JournalSnapshot { reply: tx })
                .is_err()
            {
                return;
            }
            let _ = rx.await;
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    });
    let (result, _aborted) = match tokio::time::timeout(std::time::Duration::from_secs(60), driven)
        .await
    {
        Ok(pair) => pair,
        Err(_) => {
            let rows = appender_for_dump
                .storage()
                .journal_range(0, u64::MAX)
                .await
                .unwrap_or_default();
            let kinds: Vec<String> = rows
                .iter()
                .map(|r| match &r.entry {
                    manox_harness::session::SessionTreeEntry::ToolCall { name, status, .. } => {
                        format!("tool_call:{name}:{status}")
                    }
                    manox_harness::session::SessionTreeEntry::ToolResult { .. } => {
                        "tool_result".into()
                    }
                    manox_harness::session::SessionTreeEntry::Message { .. } => "message".into(),
                    other => format!("{:?}", std::mem::discriminant(other)),
                })
                .collect();
            panic!("drive_run stalled mid-turn; journal tail: {kinds:?}");
        }
    };
    result.unwrap();
    snapshot_storm.abort();

    // Post-run: the idle arm of the actor loop services any AppendJournal
    // cmds that raced past the run's completion (the tap task lags the
    // run by scheduler granularity).
    let appender_for_drain = std::sync::Arc::clone(&journal_appender);
    while let Ok(cmd) = cmd_rx.try_recv() {
        if let SessionCmd::AppendJournal { kind, payload } = cmd
            && let Err(err) = appender_for_drain.append_typed(&kind, payload).await
        {
            tracing::warn!(%err, kind, "post-run journal append failed");
        }
    }

    let rows = journal_appender
        .storage()
        .journal_range(0, u64::MAX)
        .await
        .unwrap();
    // Every round's two tool calls must have BOTH their tap rows and their
    // persisted ToolResult messages.
    let tool_results = rows
        .iter()
        .filter(|r| {
            matches!(
                &r.entry,
                manox_harness::session::SessionTreeEntry::ToolResult { .. }
            )
        })
        .count();
    let result_messages = rows
        .iter()
        .filter(|r| {
            matches!(
                &r.entry,
                manox_harness::session::SessionTreeEntry::Message {
                    message: AgentMessage::ToolResult { .. },
                    ..
                }
            )
        })
        .count();
    let kinds: Vec<String> = rows
        .iter()
        .map(|r| match &r.entry {
            manox_harness::session::SessionTreeEntry::ToolCall { name, status, .. } => {
                format!("tool_call:{name}:{status:?}")
            }
            manox_harness::session::SessionTreeEntry::ToolResult { .. } => "tool_result".into(),
            manox_harness::session::SessionTreeEntry::Message { .. } => "message".to_string(),
            other => format!("{:?}", std::mem::discriminant(other)),
        })
        .collect();
    assert_eq!(
        tool_results, 8,
        "every parallel Grep must land a durable tool_result row; journal: {kinds:?}"
    );
    assert_eq!(
        result_messages, 8,
        "every parallel Grep must land its persisted ToolResult message; journal: {kinds:?}"
    );
}

/// A facade-level journal append (the notice tap: subagent progress,
/// retries, background tasks) arriving while a turn is in flight must
/// land in the journal IMMEDIATELY — not park for settle. The round-8
/// repro: five dispatched Sailors failed fast and their rows sat in
/// `pending_journal` while the Captain worked for 20+ minutes, so
/// followers (follow streams, webui) saw nothing live.
#[tokio::test]
async fn mid_run_journal_append_lands_immediately() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("proj");
    tokio::fs::create_dir_all(&cwd).await.unwrap();

    let stream = Arc::new(ParkOnceStream {
        started: Arc::new(tokio::sync::Notify::new()),
        release: Arc::new(tokio::sync::Notify::new()),
    });
    let stream_for_resolver = Arc::clone(&stream);
    let resolver: manox_harness::agent_loop::StreamResolver = Arc::new(move |_m: &PiModel| {
        Ok(Arc::clone(&stream_for_resolver) as Arc<dyn manox_harness::agent_loop::StreamFn>)
    });

    let mut session = create_agent_session()
        .with_cwd(&cwd)
        .with_session_dir(dir.path().join("sessions"))
        .with_agent_dir(dir.path().join("agent"))
        .with_model_runtime(ModelRuntime::new(resolver))
        .with_model(test_model())
        .with_system_prompt("You are a test assistant.")
        .build()
        .await
        .unwrap();

    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<SessionCmd>();
    let (notice_tx, _notice_rx) = mpsc::unbounded_channel::<BackendNotice>();
    let state = test_engine_state();
    let live = Arc::new(Mutex::new(LiveTranscript::default()));
    let mut run_steers = Vec::new();
    let mut shutdown_after_run = false;
    let mut pi_model = test_model();

    let handle = session.handle();
    let sessions_path = dir.path().join("sessions");
    let active_session_path = session.path().clone();
    let journal_appender = session.journal_appender();
    let run = drive_run(
        session.prompt("first turn"),
        &handle,
        &mut cmd_rx,
        &mut run_steers,
        &mut shutdown_after_run,
        live,
        &state,
        &notice_tx,
        &mut pi_model,
        &sessions_path,
        &active_session_path,
        &journal_appender,
    );

    let appender_for_probe = std::sync::Arc::clone(&journal_appender);
    let ((result, _aborted), ()) = tokio::join!(run, async {
        // The turn is parked in flight; append a facade-level row.
        stream.started.notified().await;
        cmd_tx
            .send(SessionCmd::AppendJournal {
                kind: "subagent_progress".into(),
                payload: serde_json::json!({
                    "agentId": "review-kernel",
                    "agentType": "Sailor",
                    "toolUses": 0,
                    "tokenUsage": null,
                    "latestActivity": "failed: http 402",
                    "status": "error",
                }),
            })
            .unwrap();
        // Bounded wait: the row must be readable from the shared journal
        // handle WHILE the run is still parked (nothing has settled).
        for _ in 0..10_000 {
            let rows = appender_for_probe
                .storage()
                .journal_range(0, u64::MAX)
                .await
                .unwrap_or_default();
            let found = rows.iter().any(|r| {
                matches!(
                    &r.entry,
                    manox_harness::session::SessionTreeEntry::SubagentProgress {
                        agent_id, status, ..
                    }
                        if agent_id == "review-kernel"
                            && status.eq_ignore_ascii_case("error")
                )
            });
            if found {
                // Release the parked run from INSIDE the probe: the join
                // below waits for the run too, so an outside release
                // would deadlock.
                stream.release.notify_waiters();
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("the mid-run AppendJournal must land in the journal before settle");
    });
    result.unwrap();

    // Post-settle: the chain stays linear — the foreign row is part of
    // the active chain and the run's own rows append after it.
    let rows = session
        .journal_appender()
        .storage()
        .journal_range(0, u64::MAX)
        .await
        .unwrap();
    let foreign_ix = rows
        .iter()
        .position(|r| {
            matches!(
                &r.entry,
                manox_harness::session::SessionTreeEntry::SubagentProgress { agent_id, .. }
                    if agent_id == "review-kernel"
            )
        })
        .expect("foreign row present after settle");
    // The run's own settled tail (an assistant Message row) appends
    // AFTER the foreign row: the chain stayed linear through the
    // mid-run interleaving.
    let done_ix = rows
        .iter()
        .rposition(|r| {
            matches!(
                &r.entry,
                manox_harness::session::SessionTreeEntry::Message { .. }
            )
        })
        .expect("the run's settled message rows present");
    assert!(
        foreign_ix < done_ix,
        "the mid-run row precedes the run's settled tail (linear chain)"
    );
}

/// K4 probe stream: the first call answers with a parallel tool round
/// (its assistant message materializes the deferred journal file), the
/// next call parks until the test releases it — ignoring the
/// cancellation signal like a provider stuck on the network.
struct StallAfterFirstStream {
    call: std::sync::atomic::AtomicUsize,
    parked: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl manox_harness::agent_loop::StreamFn for StallAfterFirstStream {
    async fn stream(
        &self,
        context: &manox_harness::types::AgentContext,
        _signal: tokio_util::sync::CancellationToken,
        _event_tx: tokio::sync::mpsc::Sender<manox_harness::types::AgentEvent>,
    ) -> Result<AgentMessage, anyhow::Error> {
        let n = self.call.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if n == 0 {
            return Ok(tool_rounds_assistant(context, 0));
        }
        self.parked.notify_waiters();
        self.release.notified().await;
        Ok(text_assistant(context, "done"))
    }
}

/// K4 (P0) regression: a mid-run typed journal append that PERMANENTLY
/// fails must not drop its row with a warn — the audited asymmetry let
/// the state stand in effect while the journal silently lost its entry
/// (L3); the control group (a message append failure) aborts the whole
/// run. The fail-loud tail this pins: bounded retries, a durable `error`
/// record of the loss (parked while the storage is down, landed after
/// recovery), exactly ONE ThreadEvent::Error notice to the facade (the
/// tap re-queues the notice itself as an `error` row — the kind guard
/// must break that feedback loop, not storm), and a fail-closed turn
/// K9 regression: a permanently failing UI-note append must fail LOUD
/// like every typed-append face (K4 symmetry) — the durable loss record
/// (parked for the settle/idle drains while the storage is down) and
/// exactly one facade Error notice. Pre-fix a `tracing::warn` swallowed
/// the loss: the plan/approval card vanished on reload with no journal
/// trace.
#[tokio::test]
#[cfg(unix)]
async fn ui_note_permanent_append_failure_fails_loud() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("proj");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    // build() resolves the model stream eagerly; this probe never runs a
    // turn, so the stream fn itself is never called.
    struct K9IdleStream;
    #[async_trait::async_trait]
    impl manox_harness::agent_loop::StreamFn for K9IdleStream {
        async fn stream(
            &self,
            _context: &manox_harness::types::AgentContext,
            _signal: tokio_util::sync::CancellationToken,
            _event_tx: tokio::sync::mpsc::Sender<manox_harness::types::AgentEvent>,
        ) -> Result<manox_harness::types::AgentMessage, anyhow::Error> {
            Err(anyhow::anyhow!("the k9 probe never streams"))
        }
    }
    let resolver: manox_harness::agent_loop::StreamResolver = Arc::new(|_m: &PiModel| {
        Ok(Arc::new(K9IdleStream) as Arc<dyn manox_harness::agent_loop::StreamFn>)
    });
    let session = create_agent_session()
        .with_cwd(&cwd)
        .with_session_dir(dir.path().join("sessions"))
        .with_agent_dir(dir.path().join("agent"))
        .with_model_runtime(ModelRuntime::new(resolver))
        .with_model(test_model())
        .with_system_prompt("You are a test assistant.")
        .build()
        .await
        .unwrap();
    // Materialize the journal file (deferred until now): the durable
    // user append forces the header + row to disk.
    session
        .journal_appender()
        .append_message_durable(
            manox_harness::types::AgentMessage::User {
                content: vec![manox_harness::types::ContentBlock::Text {
                    text: "k9 probe".into(),
                    signature: None,
                }],
                timestamp: chrono::Utc::now(),
                id: None,
            },
            None,
        )
        .await
        .unwrap();
    let journal_path = session.path().to_path_buf();
    // Fence the journal file: appends (open O_APPEND) fail with EACCES
    // while reads keep working.
    let original = std::fs::metadata(&journal_path)
        .unwrap()
        .permissions()
        .mode();
    std::fs::set_permissions(&journal_path, std::fs::Permissions::from_mode(0o400)).unwrap();
    struct PermGuard(std::path::PathBuf, u32);
    impl Drop for PermGuard {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(self.1));
        }
    }
    let guard = PermGuard(journal_path.clone(), original);
    if std::fs::OpenOptions::new()
        .append(true)
        .open(&journal_path)
        .is_ok()
    {
        // Root ignores the file mode — the fence is inert; skip rather
        // than assert on a false setup.
        return;
    }
    let state = test_engine_state();
    let (notice_tx, mut notice_rx) = mpsc::unbounded_channel::<BackendNotice>();
    let landed = persist_ui_note(
        &session,
        &state,
        &notice_tx,
        &UiNoteRecord {
            kind: crate::db::UiNoteKind::Notice,
            data: serde_json::json!({ "text": "doomed card" }),
        },
    )
    .await;
    assert!(!landed, "the fenced append must not report success");
    // The loss record parks for the drains (storage down ⇒
    // record_journal_loss cannot land it either).
    let parked = state.pending_journal.lock().unwrap().clone();
    assert_eq!(parked.len(), 1, "exactly one parked loss record");
    assert_eq!(parked[0].0, "error");
    let message = parked[0].1["message"].as_str().unwrap();
    assert!(
        message.contains("journal append permanently failed for `ui_note`"),
        "the loss record names the dropped face: {message}"
    );
    // Exactly one facade notice.
    match notice_rx.recv().await {
        Some(BackendNotice::Event(event)) => match *event {
            ThreadEvent::Error(err) => assert!(
                err.to_string().contains("ui_note"),
                "the facade notice names the face: {err}"
            ),
            _ => panic!("expected the Error notice, got a different event"),
        },
        None => panic!("expected the facade Error notice, got a channel close"),
        Some(_) => panic!("expected an Event notice, got a Settled/other notice"),
    }
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), notice_rx.recv())
            .await
            .is_err(),
        "the loss notice fires exactly once"
    );
    drop(guard);
}

/// cancel reported through drive_run's abort flag.
#[tokio::test]
#[cfg(unix)]
async fn mid_run_typed_append_permanent_failure_fails_loud_and_cancels() {
    use std::os::unix::fs::PermissionsExt;

    // settle_run fires the (detached) plugin `Stop` hook through the
    // global runtime handle.
    crate::runtime::init_hermetic_for_test();
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("proj");
    tokio::fs::create_dir_all(&cwd).await.unwrap();

    let stream = Arc::new(StallAfterFirstStream {
        call: std::sync::atomic::AtomicUsize::new(0),
        parked: Arc::new(tokio::sync::Notify::new()),
        release: Arc::new(tokio::sync::Notify::new()),
    });
    let stream_for_resolver = Arc::clone(&stream);
    let resolver: manox_harness::agent_loop::StreamResolver = Arc::new(move |_m: &PiModel| {
        Ok(Arc::clone(&stream_for_resolver) as Arc<dyn manox_harness::agent_loop::StreamFn>)
    });

    let mut session = create_agent_session()
        .with_cwd(&cwd)
        .with_session_dir(dir.path().join("sessions"))
        .with_agent_dir(dir.path().join("agent"))
        .with_model_runtime(ModelRuntime::new(resolver))
        .with_model(test_model())
        .with_system_prompt("You are a test assistant.")
        .build()
        .await
        .unwrap();

    // Production notice wiring, replicated: every notice rides the tap,
    // which forwards to the facade FIRST and queues durable
    // AppendJournal cmds — including the `error` row for the serializer's
    // own fail-loud notice (the feedback loop the kind guard breaks).
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<SessionCmd>();
    let (notice_tx, tap_rx) = mpsc::unbounded_channel::<BackendNotice>();
    let (tap_tx, mut facade_rx) = mpsc::unbounded_channel::<BackendNotice>();
    let tap_cmd_tx = cmd_tx.clone();
    let _tap = tokio::spawn(async move {
        let mut tap_rx = tap_rx;
        while let Some(notice) = tap_rx.recv().await {
            if let BackendNotice::Event(event) = &notice
                && let Some((kind, payload)) = durable_journal_payload(event)
            {
                let _ = tap_cmd_tx.send(SessionCmd::AppendJournal { kind, payload });
            }
            let _ = tap_tx.send(notice);
        }
    });
    let listener_tx = notice_tx.clone();
    let _listener_sub = session.subscribe(Arc::new(move |event, _cancel| {
        let tx = listener_tx.clone();
        Box::pin(async move {
            for te in crate::engine::adapt::agent_event_to_thread_events(&event) {
                let _ = tx.send(BackendNotice::Event(Box::new(te)));
            }
        })
    }));

    let state = test_engine_state();
    let live = Arc::new(Mutex::new(LiveTranscript::default()));
    let mut run_steers = Vec::new();
    let mut shutdown_after_run = false;
    let mut pi_model = test_model();
    let handle = session.handle();
    let sessions_path = dir.path().join("sessions");
    let active_session_path = session.path().clone();
    let journal_appender = session.journal_appender();
    let journal_path = session.path().to_path_buf();

    let run = drive_run(
        session.prompt("k4 fail-loud probe"),
        &handle,
        &mut cmd_rx,
        &mut run_steers,
        &mut shutdown_after_run,
        live,
        &state,
        &notice_tx,
        &mut pi_model,
        &sessions_path,
        &active_session_path,
        &journal_appender,
    );

    let skipped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ((result, aborted), ()) = tokio::join!(run, async {
        // Turn 2 is parked in the provider stream; turn 1's assistant
        // message materialized the journal file on disk.
        stream.parked.notified().await;
        // Let the serializer drain turn 1's tap rows (quiesce) so the
        // probe row is the only append in flight when the fence rises.
        let mut last = usize::MAX;
        for _ in 0..1000 {
            let rows = journal_appender
                .storage()
                .journal_range(0, u64::MAX)
                .await
                .unwrap_or_default()
                .len();
            if rows == last {
                break;
            }
            last = rows;
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        // Fence the journal file: appends (open O_APPEND) now fail with
        // EACCES while reads and the in-memory index keep working.
        let original = std::fs::metadata(&journal_path)
            .unwrap()
            .permissions()
            .mode();
        std::fs::set_permissions(&journal_path, std::fs::Permissions::from_mode(0o400)).unwrap();
        // Restore the mode even on a panicking assert.
        struct PermGuard(std::path::PathBuf, u32);
        impl Drop for PermGuard {
            fn drop(&mut self) {
                let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(self.1));
            }
        }
        let guard = PermGuard(journal_path.clone(), original);
        // Root ignores the file mode; the fence would be inert, so skip
        // rather than assert on a false setup.
        if std::fs::OpenOptions::new()
            .append(true)
            .open(&journal_path)
            .is_ok()
        {
            skipped.store(true, Ordering::SeqCst);
            drop(guard);
            stream.release.notify_waiters();
            return;
        }
        cmd_tx
            .send(SessionCmd::AppendJournal {
                kind: "subagent_progress".into(),
                payload: serde_json::json!({
                    "agentId": "k4-loss-probe",
                    "agentType": "Sailor",
                    "toolUses": 0,
                    "tokenUsage": null,
                    "latestActivity": "working",
                    "status": "running",
                }),
            })
            .unwrap();
        // The fail-loud notice must reach the facade WHILE the run is
        // still parked (bounded retries: 3 attempts, ~150ms of backoff).
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
        let mut saw_loss_notice = false;
        while tokio::time::Instant::now() < deadline && !saw_loss_notice {
            match tokio::time::timeout(std::time::Duration::from_millis(500), facade_rx.recv())
                .await
            {
                Ok(Some(BackendNotice::Event(ev))) => {
                    if let crate::thread::ThreadEvent::Error(err) = &*ev
                        && err.to_string().contains("subagent_progress")
                    {
                        saw_loss_notice = true;
                    }
                }
                Ok(Some(_)) => {}
                // The facade channel closed: no notice can still arrive.
                Ok(None) => break,
                // A quiet 500ms window is not the deadline; keep waiting
                // (the serializer's bounded retries plus scheduler load
                // can stretch the notice past one window).
                Err(_) => continue,
            }
        }
        assert!(
            saw_loss_notice,
            "a permanent mid-run append loss must notify the facade (K4 fail-loud)"
        );
        // Release the parked provider; the run converges (the serializer
        // already cancelled it fail-closed). The fence comes down right
        // after the release — on this runtime the woken stream cannot run
        // until this block yields — so the settle-side drains see a
        // healthy storage.
        stream.release.notify_waiters();
        drop(guard);
    });
    if skipped.load(Ordering::SeqCst) {
        eprintln!("skipping: running with write access despite 0o400 (root?)");
        return;
    }
    // (c) fail-closed convergence: the cancel reports through drive_run's
    // abort flag, so settle surfaces TurnFinished{cancelled}.
    assert!(
        aborted,
        "a permanent journal loss must cancel the turn fail-closed (K4)"
    );

    // Post-run: settle drains the parked loss record (the storage is
    // healthy again) — the durable `error` entry lands now, the
    // "visible after recovery" half of the K4 contract.
    settle_run(
        &result,
        aborted,
        &session,
        &state,
        &sessions_path,
        &cwd,
        &notice_tx,
        &mut run_steers,
    )
    .await;
    // Idle-arm replication for cmds that raced past the run (the tap's
    // `error` mirror of the fail-loud notice, late convergence rows).
    while let Ok(cmd) = cmd_rx.try_recv() {
        if let SessionCmd::AppendJournal { kind, payload } = cmd
            && let Err(err) = append_typed_resilient(&journal_appender, &kind, payload).await
            && let Some(row) = record_journal_loss(&journal_appender, &kind, &err).await
        {
            state.pending_journal.lock().unwrap().push(row);
        }
    }

    // (a) the loss is recorded durably: an `error` entry on disk names
    // the dropped kind, and the dropped row itself never appears — the
    // journal never pretends the state change did not happen.
    let text = tokio::fs::read_to_string(&journal_path).await.unwrap();
    assert!(
        text.contains("journal append permanently failed for `subagent_progress`"),
        "the durable loss record must be on disk after recovery: {text}"
    );
    let rows = journal_appender
        .storage()
        .journal_range(0, u64::MAX)
        .await
        .unwrap();
    assert!(
        !rows.iter().any(|r| matches!(
            &r.entry,
            manox_harness::session::SessionTreeEntry::SubagentProgress { agent_id, .. }
                if agent_id == "k4-loss-probe"
        )),
        "the permanently lost row must not appear in the journal"
    );

    // (b) exactly one fail-loud notice: the tap re-queued the notice as
    // an `error` AppendJournal whose own fenced append must NOT notify
    // again (kind guard) — no feedback storm.
    let mut loss_notices = 1; // the one observed inside the probe block
    while let Ok(notice) = facade_rx.try_recv() {
        if let BackendNotice::Event(ev) = notice
            && let crate::thread::ThreadEvent::Error(err) = &*ev
            && err
                .to_string()
                .contains("journal append permanently failed")
        {
            loss_notices += 1;
        }
    }
    assert_eq!(
        loss_notices, 1,
        "the fail-loud notice fires once per run; the error-kind guard breaks the tap loop"
    );
}

/// The user-message rows of a journal read: `(joined text blocks,
/// origin)` per entry — the K5 assertions' lens.
fn k5_user_entries(
    rows: &[manox_harness::session::jsonl::JournalRecord],
) -> Vec<(String, Option<String>)> {
    rows.iter()
        .filter_map(|r| match &r.entry {
            manox_harness::session::SessionTreeEntry::Message {
                message: AgentMessage::User { content, .. },
                origin,
                ..
            } => {
                let text = content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                Some((text, origin.clone()))
            }
            _ => None,
        })
        .collect()
}

/// Minimal drive_run wiring for the K5 tests: no tap (the assertions
/// look at message entries only), a kept-alive cmd sender so the select
/// never sees a closed channel, and a shared appender for the
/// acceptance-side writes.
struct K5Rig {
    cmd_rx: mpsc::UnboundedReceiver<SessionCmd>,
    _cmd_tx: mpsc::UnboundedSender<SessionCmd>,
    notice_tx: mpsc::UnboundedSender<BackendNotice>,
    state: Arc<EngineState>,
    live: Arc<Mutex<LiveTranscript>>,
    run_steers: Vec<String>,
    shutdown_after_run: bool,
    pi_model: PiModel,
    handle: manox_harness::harness::HarnessHandle,
    sessions_path: PathBuf,
    active_session_path: PathBuf,
    journal_appender: Arc<JournalAppender>,
}

impl K5Rig {
    fn new(dir: &tempfile::TempDir, session: &AgentSession) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<SessionCmd>();
        let (notice_tx, _notice_rx) = mpsc::unbounded_channel::<BackendNotice>();
        K5Rig {
            cmd_rx,
            _cmd_tx: cmd_tx,
            notice_tx,
            state: test_engine_state(),
            live: Arc::new(Mutex::new(LiveTranscript::default())),
            run_steers: Vec::new(),
            shutdown_after_run: false,
            pi_model: test_model(),
            handle: session.handle(),
            sessions_path: dir.path().join("sessions"),
            active_session_path: session.path().clone(),
            journal_appender: session.journal_appender(),
        }
    }
}

/// K5 (P0) regression, direct-submit path: a user entry persisted at
/// Submit acceptance (durable — a deferred session materializes on it)
/// is on disk BEFORE the run exists, the run's own user `MessageEnd`
/// records the accepted entry instead of appending a duplicate, and the
/// origin rides the accepted entry (echo retirement, §F.2).
#[tokio::test]
async fn accepted_user_entry_persists_before_the_run_and_the_middleware_skips_the_duplicate() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("proj");
    tokio::fs::create_dir_all(&cwd).await.unwrap();

    let stream = Arc::new(ToolRoundsStream {
        rounds: 0, // first call answers "done": the run completes
        call: std::sync::atomic::AtomicUsize::new(0),
    });
    let stream_for_resolver = Arc::clone(&stream);
    let resolver: manox_harness::agent_loop::StreamResolver = Arc::new(move |_m: &PiModel| {
        Ok(Arc::clone(&stream_for_resolver) as Arc<dyn manox_harness::agent_loop::StreamFn>)
    });
    let mut session = create_agent_session()
        .with_cwd(&cwd)
        .with_session_dir(dir.path().join("sessions"))
        .with_agent_dir(dir.path().join("agent"))
        .with_model_runtime(ModelRuntime::new(resolver))
        .with_model(test_model())
        .with_system_prompt("You are a test assistant.")
        .build()
        .await
        .unwrap();

    // Submit acceptance (the gateway side, replicated): the durable
    // append materializes the still-deferred session — the accepted
    // text is on disk before any run exists.
    let appender = session.journal_appender();
    let accepted_id = appender
        .append_message_durable(
            prompt_user_message("k5 accepted text", &[]),
            Some("rpc-k5".into()),
        )
        .await
        .unwrap();
    assert!(
        session.path().exists(),
        "acceptance must put the entry on disk (a deferred session materializes)"
    );

    // The actor's Prompt-cmd resolution for an accepted Submit: arm the
    // middleware skip from the accepted entry id.
    let pinned = persist_prompt_user_entry(
        &session,
        "k5 accepted text",
        &[],
        Some("rpc-k5".into()),
        Some(accepted_id.clone()),
    )
    .await
    .unwrap();
    assert_eq!(pinned.as_deref(), Some(accepted_id.as_str()));

    let mut rig = K5Rig::new(&dir, &session);
    let (result, _aborted) = drive_run(
        session.prompt("k5 accepted text"),
        &rig.handle,
        &mut rig.cmd_rx,
        &mut rig.run_steers,
        &mut rig.shutdown_after_run,
        Arc::clone(&rig.live),
        &rig.state,
        &rig.notice_tx,
        &mut rig.pi_model,
        &rig.sessions_path,
        &rig.active_session_path,
        &rig.journal_appender,
    )
    .await;
    result.unwrap();

    let rows = appender.storage().journal_range(0, u64::MAX).await.unwrap();
    let users = k5_user_entries(&rows);
    assert_eq!(
        users.len(),
        1,
        "the accepted entry is the ONLY user row (the middleware skipped its duplicate): {users:?}"
    );
    assert_eq!(users[0].0, "k5 accepted text");
    assert_eq!(
        users[0].1.as_deref(),
        Some("rpc-k5"),
        "the origin rides the accepted entry (echo retirement)"
    );
    assert_eq!(
        rows.iter()
            .filter(|r| matches!(
                &r.entry,
                manox_harness::session::SessionTreeEntry::Message {
                    message: AgentMessage::Assistant { .. },
                    ..
                }
            ))
            .count(),
        1,
        "the run completed and appended its assistant message"
    );
}

/// K5 regression, kill-after-receipt: while a turn is parked at a
/// barrier, an accepted Submit's user entry is ALREADY in the journal
/// (before the run continues); dropping everything right after the
/// acceptance — the crash window the receipt opened — leaves the entry
/// and its pinned origin on disk when the file is reopened.
#[tokio::test]
async fn kill_after_receipt_keeps_the_accepted_entry_and_origin() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("proj");
    tokio::fs::create_dir_all(&cwd).await.unwrap();

    let stream = Arc::new(ParkOnceStream {
        started: Arc::new(tokio::sync::Notify::new()),
        release: Arc::new(tokio::sync::Notify::new()),
    });
    let stream_for_resolver = Arc::clone(&stream);
    let resolver: manox_harness::agent_loop::StreamResolver = Arc::new(move |_m: &PiModel| {
        Ok(Arc::clone(&stream_for_resolver) as Arc<dyn manox_harness::agent_loop::StreamFn>)
    });
    let mut session = create_agent_session()
        .with_cwd(&cwd)
        .with_session_dir(dir.path().join("sessions"))
        .with_agent_dir(dir.path().join("agent"))
        .with_model_runtime(ModelRuntime::new(resolver))
        .with_model(test_model())
        .with_system_prompt("You are a test assistant.")
        .build()
        .await
        .unwrap();

    // Acceptance: durable append + armed skip, exactly the direct-submit
    // mechanism.
    let appender = session.journal_appender();
    let accepted_id = appender
        .append_message_durable(prompt_user_message("k5b text", &[]), Some("rpc-k5b".into()))
        .await
        .unwrap();
    persist_prompt_user_entry(
        &session,
        "k5b text",
        &[],
        Some("rpc-k5b".into()),
        Some(accepted_id),
    )
    .await
    .unwrap();

    let journal_path = session.path().clone();
    let mut rig = K5Rig::new(&dir, &session);
    // The probe wins the select and the drive_run future is dropped
    // WITHOUT releasing the park: everything after the receipt dies
    // mid-turn (the crash the receipt's window invited).
    tokio::select! {
        _ = drive_run(
            session.prompt("k5b text"),
            &rig.handle,
            &mut rig.cmd_rx,
            &mut rig.run_steers,
            &mut rig.shutdown_after_run,
            Arc::clone(&rig.live),
            &rig.state,
            &rig.notice_tx,
            &mut rig.pi_model,
            &rig.sessions_path,
            &rig.active_session_path,
            &rig.journal_appender,
        ) => panic!("the parked run must not finish before the probe"),
        _ = async {
            stream.started.notified().await;
            // The turn is parked; the run never reached completion —
            // yet the accepted entry is already durable.
            let text = tokio::fs::read_to_string(&journal_path).await.unwrap();
            assert!(
                text.contains("k5b text") && text.contains(r#""origin":"rpc-k5b""#),
                "the accepted entry with its origin must be in the journal before the run continues: {text}"
            );
            assert_eq!(
                text.matches("k5b text").count(),
                1,
                "the middleware skip kept the announced message from duplicating the entry: {text}"
            );
        } => {}
    }

    // Kill-after-receipt: everything is dropped (the select consumed the
    // run future); reopen the file — the entry and origin survived.
    let text = tokio::fs::read_to_string(&journal_path).await.unwrap();
    assert!(
        text.contains("k5b text") && text.contains(r#""origin":"rpc-k5b""#),
        "the accepted entry must survive the killed run: {text}"
    );
    let reopened = manox_harness::session::jsonl::JsonlSessionStorage::open(&journal_path)
        .await
        .unwrap();
    let rows = reopened.journal_range(0, u64::MAX).await.unwrap();
    let users = k5_user_entries(&rows);
    assert_eq!(users.len(), 1, "reopened journal: {users:?}");
    assert_eq!(users[0].0, "k5b text");
    assert_eq!(users[0].1.as_deref(), Some("rpc-k5b"));
}

/// K5 regression, queued-submit path: a Submit whose acceptance did not
/// persist (the engine was running; the gateway queued it) persists at
/// drain — in the actor's Prompt handler, BEFORE the run starts, so
/// before model-visible — and the run's middleware skips the duplicate.
#[tokio::test]
async fn queued_submit_persists_at_drain_before_the_run() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("proj");
    tokio::fs::create_dir_all(&cwd).await.unwrap();

    let stream = Arc::new(ToolRoundsStream {
        rounds: 0,
        call: std::sync::atomic::AtomicUsize::new(0),
    });
    let stream_for_resolver = Arc::clone(&stream);
    let resolver: manox_harness::agent_loop::StreamResolver = Arc::new(move |_m: &PiModel| {
        Ok(Arc::clone(&stream_for_resolver) as Arc<dyn manox_harness::agent_loop::StreamFn>)
    });
    let mut session = create_agent_session()
        .with_cwd(&cwd)
        .with_session_dir(dir.path().join("sessions"))
        .with_agent_dir(dir.path().join("agent"))
        .with_model_runtime(ModelRuntime::new(resolver))
        .with_model(test_model())
        .with_system_prompt("You are a test assistant.")
        .build()
        .await
        .unwrap();

    // The actor's drain of a queued Submit: no accepted_entry, an
    // origin → persist NOW (durable), before drive_run exists.
    let drained_id =
        persist_prompt_user_entry(&session, "drained text", &[], Some("rpc-q".into()), None)
            .await
            .unwrap()
            .expect("an origin-carrying prompt persists at drain");

    let appender = session.journal_appender();
    let rows_before = appender.storage().journal_range(0, u64::MAX).await.unwrap();
    let users_before = k5_user_entries(&rows_before);
    assert_eq!(
        users_before,
        vec![("drained text".to_string(), Some("rpc-q".into()))],
        "the drained Submit must be in the journal BEFORE the run starts"
    );

    let mut rig = K5Rig::new(&dir, &session);
    let (result, _aborted) = drive_run(
        session.prompt("drained text"),
        &rig.handle,
        &mut rig.cmd_rx,
        &mut rig.run_steers,
        &mut rig.shutdown_after_run,
        Arc::clone(&rig.live),
        &rig.state,
        &rig.notice_tx,
        &mut rig.pi_model,
        &rig.sessions_path,
        &rig.active_session_path,
        &rig.journal_appender,
    )
    .await;
    result.unwrap();

    let rows = appender.storage().journal_range(0, u64::MAX).await.unwrap();
    let users = k5_user_entries(&rows);
    assert_eq!(users.len(), 1, "no duplicate user row: {users:?}");
    assert_eq!(users[0].1.as_deref(), Some("rpc-q"));
    let ids: Vec<&str> = rows
        .iter()
        .filter(|r| {
            matches!(
                &r.entry,
                manox_harness::session::SessionTreeEntry::Message {
                    message: AgentMessage::User { .. },
                    ..
                }
            )
        })
        .map(|r| r.entry.id())
        .collect();
    assert_eq!(
        ids,
        vec![drained_id.as_str()],
        "the surviving user row IS the drain-time entry (the middleware recorded it, never re-appended)"
    );
}

/// K5 guard: a pin no run consumed (its run died before announcing the
/// user message) must never leak the middleware skip into a later turn —
/// even when the later turn's user content matches the stale pin
/// exactly. The drain-time resolution clears the pin before arming (or
/// declining to arm) its own.
#[tokio::test]
async fn stale_accepted_pin_never_leaks_into_the_next_turn() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("proj");
    tokio::fs::create_dir_all(&cwd).await.unwrap();

    let stream = Arc::new(ToolRoundsStream {
        rounds: 0,
        call: std::sync::atomic::AtomicUsize::new(0),
    });
    let stream_for_resolver = Arc::clone(&stream);
    let resolver: manox_harness::agent_loop::StreamResolver = Arc::new(move |_m: &PiModel| {
        Ok(Arc::clone(&stream_for_resolver) as Arc<dyn manox_harness::agent_loop::StreamFn>)
    });
    let mut session = create_agent_session()
        .with_cwd(&cwd)
        .with_session_dir(dir.path().join("sessions"))
        .with_agent_dir(dir.path().join("agent"))
        .with_model_runtime(ModelRuntime::new(resolver))
        .with_model(test_model())
        .with_system_prompt("You are a test assistant.")
        .build()
        .await
        .unwrap();

    // A stale pin whose content MATCHES the next turn's user message
    // (the worst case for a leaked skip): a dead run left it armed.
    let appender = session.journal_appender();
    let stale_content = match prompt_user_message("fresh text", &[]) {
        AgentMessage::User { content, .. } => serde_json::to_value(content).unwrap(),
        _ => unreachable!("prompt_user_message builds a user message"),
    };
    appender.pin_accepted_user_entry("stale-entry-id".into(), stale_content);

    // A legacy (origin-less) prompt: the drain-time resolution declines
    // to persist — and clears the stale pin on its way through.
    let pinned = persist_prompt_user_entry(&session, "fresh text", &[], None, None)
        .await
        .unwrap();
    assert!(
        pinned.is_none(),
        "a legacy prompt keeps the middleware path"
    );

    let mut rig = K5Rig::new(&dir, &session);
    let (result, _aborted) = drive_run(
        session.prompt("fresh text"),
        &rig.handle,
        &mut rig.cmd_rx,
        &mut rig.run_steers,
        &mut rig.shutdown_after_run,
        Arc::clone(&rig.live),
        &rig.state,
        &rig.notice_tx,
        &mut rig.pi_model,
        &rig.sessions_path,
        &rig.active_session_path,
        &rig.journal_appender,
    )
    .await;
    result.unwrap();

    let rows = appender.storage().journal_range(0, u64::MAX).await.unwrap();
    let users = k5_user_entries(&rows);
    assert_eq!(users.len(), 1, "the legacy turn appended its own user row");
    assert_eq!(users[0].0, "fresh text");
    assert_eq!(users[0].1, None);
    let id = rows
        .iter()
        .find(|r| {
            matches!(
                &r.entry,
                manox_harness::session::SessionTreeEntry::Message {
                    message: AgentMessage::User { .. },
                    ..
                }
            )
        })
        .map(|r| r.entry.id().to_string())
        .unwrap();
    assert_ne!(
        id, "stale-entry-id",
        "the surviving row is the middleware's fresh append, not the stale pin's id"
    );
}

/// `AppendUiNote` arriving while a turn is in flight must not be dropped
/// like the other unserviceable mid-run commands: the card merges into
/// the mirror immediately (a mid-run switch-back renders it) and parks
/// its persistence for the idle loop, which appends the `custom` entry
/// at the leaf once the run owns no borrow of the session.
#[tokio::test]
async fn mid_run_append_ui_note_mirrors_now_and_parks_persist() {
    // The settle path fires plugin hooks, and Registry::fire takes the
    // global runtime handle unconditionally — without this the test
    // only passes when an earlier test in the binary happened to init
    // the runtime (pre-existing isolation fragility, HEAD-verified:
    // alone it panics "tokio runtime not initialized").
    crate::runtime::init_hermetic_for_test();
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("proj");
    tokio::fs::create_dir_all(&cwd).await.unwrap();

    let stream = Arc::new(MidRunModelStream {
        calls: std::sync::atomic::AtomicUsize::new(0),
        seen: Arc::new(std::sync::Mutex::new(Vec::new())),
        turn1_started: Arc::new(tokio::sync::Notify::new()),
        release1: Arc::new(tokio::sync::Notify::new()),
        turn2_started: Arc::new(tokio::sync::Notify::new()),
        release2: Arc::new(tokio::sync::Notify::new()),
    });
    let stream_for_resolver = Arc::clone(&stream);
    let resolver: manox_harness::agent_loop::StreamResolver = Arc::new(move |_m: &PiModel| {
        Ok(Arc::clone(&stream_for_resolver) as Arc<dyn manox_harness::agent_loop::StreamFn>)
    });
    let runtime = ModelRuntime::new(resolver);

    let mut session = create_agent_session()
        .with_cwd(&cwd)
        .with_session_dir(dir.path().join("sessions"))
        .with_agent_dir(dir.path().join("agent"))
        .with_model_runtime(runtime)
        .with_model(test_model())
        .with_tools(vec![
            Arc::new(EchoTool) as Arc<dyn manox_harness::tool::AgentTool>
        ])
        .with_system_prompt("You are a test assistant.")
        .build()
        .await
        .unwrap();

    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<SessionCmd>();
    let (notice_tx, _notice_rx) = mpsc::unbounded_channel::<BackendNotice>();
    let state = test_engine_state();
    let live = Arc::new(Mutex::new(LiveTranscript::default()));
    let mut run_steers = Vec::new();
    let mut shutdown_after_run = false;
    let mut pi_model = test_model();

    let handle = session.handle();
    let sessions_path = dir.path().join("sessions");
    let active_session_path = session.path().clone();
    let journal_appender = session.journal_appender();
    let run = drive_run(
        session.prompt("first turn"),
        &handle,
        &mut cmd_rx,
        &mut run_steers,
        &mut shutdown_after_run,
        live,
        &state,
        &notice_tx,
        &mut pi_model,
        &sessions_path,
        &active_session_path,
        &journal_appender,
    );
    let record = UiNoteRecord {
        kind: crate::db::UiNoteKind::Notice,
        data: serde_json::json!({ "text": "mid-run card" }),
    };

    let ((result, _aborted), ()) = tokio::join!(run, async {
        stream.turn1_started.notified().await;
        cmd_tx.send(SessionCmd::AppendUiNote(record)).unwrap();
        // The mirror takes the card while the turn is still in flight.
        for _ in 0..10_000 {
            if state.notes.lock().unwrap().len() == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            state.notes.lock().unwrap().len(),
            1,
            "mid-run AppendUiNote must merge into the mirror immediately"
        );
        assert!(
            matches!(
                state.history.lock().unwrap().last(),
                Some(HistoryEntry::Note(_))
            ),
            "the mirrored tail is the note"
        );
        stream.release1.notify_waiters();
        stream.turn2_started.notified().await;
        stream.release2.notify_waiters();
    });
    // Settlement persists the parked note BEFORE the authoritative
    // sync, so the rebuilt mirror retains it — no loss window between
    // the mid-run mirror and the next reload.
    settle_run(
        &result,
        false,
        &session,
        &state,
        &sessions_path,
        &cwd,
        &notice_tx,
        &mut run_steers,
    )
    .await;
    result.unwrap();

    assert!(
        state.pending_ui_notes.lock().unwrap().is_empty(),
        "settlement drains the parked queue"
    );
    assert!(
        matches!(
            state.history.lock().unwrap().last(),
            Some(HistoryEntry::Note(_))
        ),
        "the authoritative rebuild must retain the parked note"
    );
    assert_eq!(
        state.notes.lock().unwrap().len(),
        1,
        "the positioned-note mirror survives settlement"
    );
    let jsonl = tokio::fs::read_to_string(session.path()).await.unwrap();
    assert!(
        jsonl.contains("\"customType\":\"manox_ui_note\"") && jsonl.contains("mid-run card"),
        "settlement must persist the parked note"
    );
}

/// A stream that answers every provider request immediately with a
/// terminal assistant message carrying the request's model identity.
struct StaticStream;

#[async_trait::async_trait]
impl manox_harness::agent_loop::StreamFn for StaticStream {
    async fn stream(
        &self,
        context: &manox_harness::types::AgentContext,
        _signal: tokio_util::sync::CancellationToken,
        _event_tx: tokio::sync::mpsc::Sender<manox_harness::types::AgentEvent>,
    ) -> Result<manox_harness::types::AgentMessage, anyhow::Error> {
        Ok(AgentMessage::Assistant {
            content: vec![ContentBlock::Text {
                text: "ok".into(),
                signature: None,
            }],
            model: context.model.id.clone(),
            provider: context.model.provider.clone(),
            api: context.model.api.clone(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            raw_stop_reason: None,
            stop_reason: Some(manox_harness::types::StopReason::Stop),
            usage: Box::new(manox_harness::types::Usage::default()),
            error_message: None,
            timestamp: chrono::Utc::now(),
        })
    }
}

/// Restores the two test models from their session references.
struct TestModelCatalog;

impl manox_harness::coding_agent::model_runtime::ModelCatalog for TestModelCatalog {
    fn resolve(&self, provider: &str, model_id: &str) -> Option<PiModel> {
        match (provider, model_id) {
            ("test", "test") => Some(test_model()),
            ("test", "new") => Some(test_model_switched()),
            _ => None,
        }
    }
}

/// A reopened session must project its own persisted model onto the
/// harness: the restore path builds with no model override so
/// `builder.open()` restores the session model, and
/// `adopt_session_model` mirrors it into the actor's working model and
/// the shared slot. Regression for the reopen that showed the default
/// model in the composer selector.
#[tokio::test]
async fn reopened_session_restores_its_own_model() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("proj");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let sessions = dir.path().join("sessions");
    let agent = dir.path().join("agent");
    tokio::fs::create_dir_all(&sessions).await.unwrap();
    tokio::fs::create_dir_all(&agent).await.unwrap();

    let resolver: manox_harness::agent_loop::StreamResolver = Arc::new(|_m: &PiModel| {
        Ok(Arc::new(StaticStream) as Arc<dyn manox_harness::agent_loop::StreamFn>)
    });
    let runtime = ModelRuntime::new(resolver).with_catalog(Arc::new(TestModelCatalog));

    // Phase 1: a session that ran under `test` and switched to `new`.
    let mut session = create_agent_session()
        .with_cwd(&cwd)
        .with_session_dir(&sessions)
        .with_agent_dir(&agent)
        .with_model_runtime(runtime.clone())
        .with_model(test_model())
        .with_tools(vec![
            Arc::new(EchoTool) as Arc<dyn manox_harness::tool::AgentTool>
        ])
        .with_system_prompt("You are a test assistant.")
        .build()
        .await
        .unwrap();
    let path = session.path().clone();
    // A prompt materializes the JSONL (the deferred-first contract
    // writes disk only once an assistant message exists).
    let _ = session.prompt("first turn").await.unwrap();
    session.set_model(test_model_switched()).await.unwrap();
    let jsonl = tokio::fs::read_to_string(&path).await.unwrap();
    assert!(
        jsonl.contains("\"type\":\"model_change\"") && jsonl.contains("\"modelId\":\"new\""),
        "the switch must persist a model_change entry for the new model"
    );
    session.close().await.unwrap();

    // Phase 2: the fixed restore path — no model override on the
    // builder, so `open()` restores the session's own model.
    let reopened = create_agent_session()
        .with_agent_dir(&agent)
        .with_model_runtime(runtime)
        .with_tools(vec![
            Arc::new(EchoTool) as Arc<dyn manox_harness::tool::AgentTool>
        ])
        .with_system_prompt("You are a test assistant.")
        .open(path)
        .await
        .unwrap();
    assert_eq!(
        reopened.model().id,
        "new",
        "the reopened session must restore its own model, not the default"
    );

    let state = test_engine_state();
    let mut pi_model = test_model();
    adopt_session_model(&reopened, &mut pi_model, &state);
    assert_eq!(
        pi_model.id, "new",
        "the actor's working model follows the restored session"
    );
    assert_eq!(
        state.model.lock().unwrap().as_ref().map(|m| m.id.as_str()),
        Some("new"),
        "the shared model slot follows the restored session"
    );
}

#[tokio::test]
async fn reasoning_effort_sidecar_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let session = dir.path().join("sess-effort.jsonl");

    // Fresh session: no sidecar -> default High.
    assert_eq!(
        load_reasoning_effort(dir.path(), &session).await,
        ReasoningEffort::High
    );

    write_reasoning_effort_sidecar(dir.path(), &session, ReasoningEffort::Max)
        .await
        .unwrap();
    assert_eq!(
        load_reasoning_effort(dir.path(), &session).await,
        ReasoningEffort::Max
    );

    write_reasoning_effort_sidecar(dir.path(), &session, ReasoningEffort::High)
        .await
        .unwrap();
    assert_eq!(
        load_reasoning_effort(dir.path(), &session).await,
        ReasoningEffort::High
    );
}

#[tokio::test]
async fn reasoning_effort_sidecar_tolerates_unknown_values() {
    let dir = tempfile::tempdir().unwrap();
    let session = dir.path().join("sess-effort-unknown.jsonl");
    let meta = manox_harness::session_meta::SessionMeta {
        reasoning_effort: Some("medium".to_string()),
        ..Default::default()
    };
    manox_harness::session_meta::save(dir.path(), &session, &meta)
        .await
        .unwrap();
    assert_eq!(
        load_reasoning_effort(dir.path(), &session).await,
        ReasoningEffort::High,
        "unknown persisted efforts fall back to the default"
    );
}

fn assistant_usage(cache_read: u64) -> manox_harness::types::Usage {
    manox_harness::types::Usage {
        input_tokens: 10,
        cache_read_input_tokens: cache_read,
        ..Default::default()
    }
}

fn assistant_request(usage: manox_harness::types::Usage) -> AgentMessage {
    AgentMessage::Assistant {
        content: Vec::new(),
        model: "deepseek-v4-flash".into(),
        provider: "DeepSeek".into(),
        api: "anthropic".into(),
        response_model: None,
        response_id: None,
        diagnostics: None,
        stop_reason: Some(manox_harness::types::StopReason::Stop),
        raw_stop_reason: None,
        usage: Box::new(usage),
        error_message: None,
        timestamp: chrono::Utc::now(),
    }
}

#[test]
fn request_attribution_keys_turns_and_keeps_only_latest_request_per_model() {
    let (u1, u2, u3) = (
        assistant_usage(100),
        assistant_usage(200),
        assistant_usage(300),
    );
    let messages = vec![
        AgentMessage::User {
            content: Vec::new(),
            timestamp: chrono::Utc::now(),
            id: None,
        },
        assistant_request(u1.clone()),
        assistant_request(u2.clone()),
        AgentMessage::User {
            content: Vec::new(),
            timestamp: chrono::Utc::now(),
            id: None,
        },
        assistant_request(u3.clone()),
    ];
    let history: Vec<HistoryEntry> = adapt::harness_messages_to_messages(&messages)
        .into_iter()
        .map(HistoryEntry::Message)
        .collect();
    let (per_turn, per_model_last) = request_attribution(&history, &messages);
    let id = |ix: usize| match &history[ix] {
        HistoryEntry::Message(m) => m.id.clone(),
        _ => String::new(),
    };

    // Each turn accumulates under its own triggering user message.
    assert_eq!(per_turn.len(), 2);
    assert_eq!(per_turn[&id(0)], to_token_usage(&u1) + to_token_usage(&u2));
    assert_eq!(per_turn[&id(3)], to_token_usage(&u3));

    // The budget numerator is the single latest request, never a sum.
    let last = per_model_last
        .get("DeepSeek/deepseek-v4-flash")
        .expect("latest request keyed by provider/model");
    assert_eq!(last.cache_read_input_tokens, 300);
}

#[test]
#[should_panic(expected = "out of lockstep")]
fn request_attribution_backstop_trips_on_leftover_mapped_rows() {
    let messages = vec![
        AgentMessage::User {
            content: Vec::new(),
            timestamp: chrono::Utc::now(),
            id: None,
        },
        assistant_request(assistant_usage(100)),
    ];
    let mut history: Vec<HistoryEntry> = adapt::harness_messages_to_messages(&messages)
        .into_iter()
        .map(HistoryEntry::Message)
        .collect();
    // Simulate adapt drift: one mapped row the walk never consumes.
    history.push(history[0].clone());
    let _ = request_attribution(&history, &messages);
}

#[test]
#[should_panic(expected = "out of lockstep")]
fn request_attribution_backstop_trips_on_over_consumption() {
    let messages = vec![
        AgentMessage::User {
            content: Vec::new(),
            timestamp: chrono::Utc::now(),
            id: None,
        },
        assistant_request(assistant_usage(100)),
    ];
    // Simulate adapt drift in the other direction: the walk consumes an
    // id for a message the mapping would not emit.
    let history: Vec<HistoryEntry> = adapt::harness_messages_to_messages(&messages[..1])
        .into_iter()
        .map(HistoryEntry::Message)
        .collect();
    let _ = request_attribution(&history, &messages);
}

#[test]
fn delegation_tool_description_carries_definition_and_contract() {
    let rendered = crate::subagent::delegation_description(
        "Read-only codebase exploration",
        "read-only",
        "Explore",
    );
    assert!(
        rendered.contains("[capability: read-only]"),
        "capability tag present: {rendered}"
    );
    assert!(
        rendered.contains("`Explore`"),
        "definition name present: {rendered}"
    );
    assert!(
        rendered.contains("fresh-context"),
        "description declares the fresh-context boundary: {rendered}"
    );
    assert!(
        rendered.contains("run_in_background"),
        "description declares the background mode: {rendered}"
    );
    assert!(
        rendered.contains("auto-rejected"),
        "description declares the never-approval boundary: {rendered}"
    );
}

/// The capability tag distinguishes write+bash defs (Sailor) from
/// read-only defs (Explore). It is the routing key for the plan-mode
/// read-only resolver, which blocks write/bash subagents from being
/// dispatched while plan mode is active.
#[test]
fn subagent_capability_tags_write_bash_vs_read_only() {
    let mut registry = manox_harness::ext_point_agent::AgentRegistry::new();
    manox_harness::subagent::spawn::register_defaults(&mut registry);
    let sailor = registry.get("Sailor").expect("Sailor registered");
    let explore = registry.get("Explore").expect("Explore registered");
    assert_eq!(subagent_capability(sailor), "write+bash");
    assert_ne!(
        subagent_capability(sailor),
        "read-only",
        "Sailor routes async (not read-only)"
    );
    assert_eq!(
        subagent_capability(explore),
        "read-only",
        "Explore stays synchronous"
    );
}

#[test]
fn subagent_bash_description_forbids_background_and_drops_host_claims() {
    // The subagent Bash wrapper rejects `run_in_background` (N1: no
    // bare-spawn escape hatch) and its description must not carry the
    // host-session BashTool's false claims (state persists / approval).
    assert!(
        SUBAGENT_BASH_DESCRIPTION.contains("run_in_background"),
        "description flags background refusal"
    );
    assert!(
        !SUBAGENT_BASH_DESCRIPTION.contains("State persists"),
        "no state-persistence claim"
    );
    assert!(
        !SUBAGENT_BASH_DESCRIPTION.contains("requires user approval"),
        "no approval-gating claim for the ungated subagent session"
    );
}

/// `SubagentBashTool` must reject `run_in_background` (N1: no bare-spawn
/// escape hatch) while leaving foreground calls untouched. Uses a marker
/// inner so the gate is exercised without constructing a real BashTool.
struct MarkerBash;
#[async_trait::async_trait]
impl manox_harness::tool::AgentTool for MarkerBash {
    fn name(&self) -> &str {
        "Bash"
    }
    fn description(&self) -> &str {
        "marker"
    }
    fn is_read_only(&self) -> bool {
        false
    }
    fn requires_approval(&self, _: &serde_json::Value) -> bool {
        false
    }
    fn parameters_schema(&self) -> serde_json::Value {
        // Mirror the kernel BashTool's shape so the wrapper's strip is
        // exercised (run_in_background + unsandboxed get removed).
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {"type": "string"},
                "run_in_background": {"type": "boolean"},
                "unsandboxed": {"type": "boolean"}
            },
            "required": ["command"]
        })
    }
    async fn execute(
        &self,
        _: &str,
        _: serde_json::Value,
        _: tokio_util::sync::CancellationToken,
        _: &dyn manox_harness::tool::ToolContext,
    ) -> Result<manox_harness::tool::AgentToolResult, manox_harness::tool::ToolError> {
        Ok(manox_harness::tool::AgentToolResult::text("foreground-ok"))
    }
}

#[tokio::test]
async fn subagent_bash_refuses_background_but_allows_foreground() {
    use std::path::PathBuf;
    let tool = SubagentBashTool {
        inner: Arc::new(MarkerBash),
    };
    let ctx = manox_harness::tool::LocalToolContext::new(
        Arc::new(manox_harness::env::TokioExecutionEnv::new(PathBuf::from(
            "/tmp",
        ))),
        PathBuf::from("/tmp"),
        Arc::new(manox_harness::tool::ToolState::new()),
    );
    // Background: refused at the gate; the inner is never reached.
    let bg = tool
        .execute(
            "c1",
            serde_json::json!({"command": "echo hi", "run_in_background": true}),
            tokio_util::sync::CancellationToken::new(),
            &ctx,
        )
        .await;
    let err = bg.unwrap_err().to_string();
    assert!(
        err.contains("not available inside a subagent"),
        "background refused: {err}"
    );
    // Foreground: the gate passes and the inner runs.
    let fg = tool
        .execute(
            "c2",
            serde_json::json!({"command": "echo hi"}),
            tokio_util::sync::CancellationToken::new(),
            &ctx,
        )
        .await
        .expect("foreground gate passes");
    let fg_text: String = fg
        .content
        .iter()
        .filter_map(|b| {
            if let manox_harness::types::ContentBlock::Text { text, .. } = b {
                Some(text.as_str())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(fg_text, "foreground-ok", "foreground reached the inner");
}

#[test]
fn subagent_bash_schema_strips_background_and_escalation_fields() {
    let tool = SubagentBashTool {
        inner: Arc::new(MarkerBash),
    };
    let schema = tool.parameters_schema();
    let props = schema["properties"]
        .as_object()
        .expect("schema has properties");
    assert!(
        !props.contains_key("run_in_background"),
        "run_in_background stripped from the subagent schema"
    );
    assert!(
        !props.contains_key("sandbox_permissions"),
        "sandbox_permissions stripped from the subagent schema"
    );
    assert!(
        !props.contains_key("justification"),
        "justification stripped from the subagent schema"
    );
    assert!(props.contains_key("command"), "command still advertised");
}

/// A dispatched subagent session persists under the provider's
/// host-injected session directory, and the header carries the dispatch
/// lineage (definition name + parent) from the descriptor. Without an
/// injected directory the transcript lives in a provider-internal
/// tempdir removed at settle — the lifecycle is no longer the caller's
/// to manage.
#[tokio::test]
async fn subagent_session_persists_under_host_dir_with_metadata() {
    use manox_harness::subagent::{SpawnProvider, SubagentRuntime};

    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("proj");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let resolver: manox_harness::agent_loop::StreamResolver = Arc::new(|_m: &PiModel| {
        Ok(Arc::new(StaticStream) as Arc<dyn manox_harness::agent_loop::StreamFn>)
    });
    let runtime = ModelRuntime::new(resolver);

    let tools: Vec<Arc<dyn manox_harness::tool::AgentTool>> = vec![
        Arc::new(manox_harness::tools::read::ReadTool),
        Arc::new(manox_harness::tools::grep::GrepTool),
        Arc::new(manox_harness::tools::glob::GlobTool),
        Arc::new(manox_harness::tools::ls::LsTool),
    ];

    // Host-injected directory: the transcript persists there and the
    // header carries the dispatch lineage.
    let subagents = dir.path().join("subagents");
    let subagent_runtime = SubagentRuntime::new();
    let provider = SpawnProvider::new(tools.clone())
        .with_model_runtime(runtime.clone())
        .with_model(test_model())
        .with_session_dir(subagents.clone());
    subagent_runtime.register_provider_permanent(Arc::new(provider));
    let mut request = manox_harness::subagent::test_request("explore the manifest", "hi");
    request.kind = "Explore".into();
    request.persona = Some("read-only codebase explorer".into());
    request.parent_session = Some("thread-parent".into());
    request.cwd = cwd.clone();
    request.env = Arc::new(manox_harness::env::TokioExecutionEnv::new(cwd.clone()));
    let run = subagent_runtime
        .start("spawn", request)
        .await
        .expect("dispatch starts");
    let result = run.result().await;
    assert_eq!(
        result.stop_reason,
        manox_harness::subagent::StopReason::Completed,
        "the child completes: {result:?}"
    );
    let mut entries = tokio::fs::read_dir(&subagents).await.unwrap();
    let mut header = None;
    while let Some(entry) = entries.next_entry().await.unwrap() {
        let content = tokio::fs::read_to_string(entry.path()).await.unwrap();
        header = Some(content.lines().next().unwrap().to_string());
    }
    let header = header.expect("one transcript under the host dir");
    assert!(
        header.contains("\"metadata\":{\"subagent\":{")
            && header.contains("\"type\":\"Explore\"")
            && header.contains("\"parent\":\"thread-parent\""),
        "header must carry the subagent lineage: {header}"
    );
}

// ── K3/K2/K1: decision-point entries, journal authority, replay gate ──

/// One entry per replay-supported journal kind, in the payload shapes
/// the production write faces emit (`durable_journal_payload` and the
/// typed-append call sites). The K1 regression drives these through the
/// real append face and asserts the coverage list below against the
/// resulting chain.
fn scripted_state_rows() -> Vec<(&'static str, serde_json::Value)> {
    use serde_json::json;
    vec![
        ("turn_start", json!({})),
        ("agent_text_delta", json!({ "delta": "hel" })),
        ("agent_thinking_delta", json!({ "delta": "thinking..." })),
        (
            "tool_call",
            json!({ "callId": "call-1", "name": "Read", "title": "Read a file", "status": "pending_approval", "input": { "path": "/tmp/x" } }),
        ),
        (
            "approval",
            json!({ "kind": "request", "authId": "call-1", "payload": { "toolName": "Read", "summary": "s", "input": {} } }),
        ),
        (
            "approval",
            json!({ "kind": "decision", "authId": "call-1", "payload": { "toolName": "Read", "verdict": "allow_once" } }),
        ),
        (
            "tool_result",
            json!({ "callId": "call-1", "output": "ok", "isError": false }),
        ),
        (
            "tool_output_chunk",
            json!({ "callId": "call-1", "chunk": "stream" }),
        ),
        (
            "subagent_child",
            json!({ "agentId": "agent-1", "event": { "type": "started", "subagentType": "Sailor", "description": "d", "childId": "c-1" } }),
        ),
        (
            "subagent_progress",
            json!({ "agentId": "agent-1", "agentType": "Sailor", "toolUses": 2, "tokenUsage": null, "latestActivity": "working", "status": "running" }),
        ),
        (
            "retry",
            json!({ "attempt": 1, "maxAttempts": 3, "delaySecs": 2, "reason": "overloaded", "detail": null }),
        ),
        (
            "model_change",
            json!({ "provider": "test", "modelId": "replay-model" }),
        ),
        ("cwd_change", json!({ "cwd": "/replay/work" })),
        ("project_change", json!({ "path": "/replay/proj" })),
        (
            "permission_mode_change",
            json!({ "mode": "danger-full-access" }),
        ),
        ("plan_mode_change", json!({ "enabled": true })),
        (
            "plan_update",
            json!({ "snapshot": [{ "content": "step one", "status": "pending", "activeForm": "stepping" }] }),
        ),
        (
            "plan_review",
            json!({ "state": "proposed", "planFile": "/replay/plan.md" }),
        ),
        ("goal", json!({ "goal": { "objective": "replay" } })),
        ("title", json!({ "title": "replayed title" })),
        ("browser_suites", json!({ "suites": ["chrome_use"] })),
        (
            "background_task",
            json!({ "snapshot": { "taskId": "task-1", "status": "running" } }),
        ),
        (
            "pinned_archived",
            json!({ "pinned": true, "archived": false }),
        ),
        (
            "pinned_archived",
            json!({ "pinned": false, "archived": true }),
        ),
        ("compaction_started", json!({ "tokensBefore": 1234 })),
        (
            "metrics",
            json!({ "metricType": "prefix_stability", "data": { "stabilityPct": 99, "systemChanged": false, "toolsChanged": false } }),
        ),
        ("stop", json!({ "reason": "end_turn" })),
        (
            "turn_finish",
            json!({ "cancelled": false, "failed": false, "strandedSteerIds": [] }),
        ),
        ("error", json!({ "message": "scripted error row" })),
    ]
}

/// K5 edge: the middleware pin (and the queued-submit drain
/// persistence) carries the POST-expansion text — the shape the run
/// announces — so a slash-command prompt's content-match skip holds
/// and the journal never double-entries. The raw text must NOT match
/// the pin.
#[tokio::test]
async fn prompt_pin_carries_the_expanded_text() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("proj");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let stream = Arc::new(ToolRoundsStream {
        rounds: 0,
        call: std::sync::atomic::AtomicUsize::new(0),
    });
    let resolver: manox_harness::agent_loop::StreamResolver = Arc::new(move |_m: &PiModel| {
        Ok(Arc::clone(&stream) as Arc<dyn manox_harness::agent_loop::StreamFn>)
    });
    let resources = manox_harness::harness::HarnessResources {
        prompt_templates: vec![manox_harness::harness::PromptTemplate {
            name: "deploy".into(),
            content: "deploy $ARGUMENTS now".into(),
        }],
        ..Default::default()
    };
    let session = create_agent_session()
        .with_cwd(&cwd)
        .with_session_dir(dir.path().join("sessions"))
        .with_agent_dir(dir.path().join("agent"))
        .with_model_runtime(ModelRuntime::new(resolver))
        .with_model(test_model())
        .with_system_prompt("You are a test assistant.")
        .with_resources(resources)
        .build()
        .await
        .unwrap();

    let id =
        persist_prompt_user_entry(&session, "/deploy service", &[], Some("rpc-x".into()), None)
            .await
            .expect("the origin path persists")
            .expect("an entry id comes back");
    let expanded =
        manox_harness::harness::expand_prompt_with(session.resources(), "/deploy service");
    assert_eq!(expanded, "deploy service now");
    let content_of = |text: &str| {
        serde_json::to_value(match prompt_user_message(text, &[]) {
            manox_harness::types::AgentMessage::User { content, .. } => content,
            _ => unreachable!("prompt_user_message builds a user message"),
        })
        .unwrap()
    };
    let appender = session.journal_appender();
    assert!(
        appender
            .take_accepted_user_entry(&content_of("/deploy service"))
            .is_none(),
        "the raw text must not match the pin"
    );
    assert_eq!(
        appender
            .take_accepted_user_entry(&content_of(&expanded))
            .as_deref(),
        Some(id.as_str()),
        "the expanded announce consumes the pin"
    );
}

/// Every on-disk `type` tag the replay-consistency regression must
/// cover: the full state-change vocabulary the fold and restore
/// rebuild consume, the transcript + lifecycle + delta kinds the
/// display projection consumes, and the two kernel-written faces
/// (`thinking_level_change`, `active_tools_change`). Tree-management
/// kinds (`leaf`, `label`, `session_info`, `branch_summary`,
/// `custom_message`) and the dedicated-path kinds (`message` is
/// covered by the real turn; `compaction` owns a richer append path
/// the typed face refuses by design) are out of the scripted set.
const REPLAY_COVERAGE_KINDS: &[&str] = &[
    "message",
    "turn_start",
    "turn_finish",
    "stop",
    "retry",
    "error",
    "agent_text_delta",
    "agent_thinking_delta",
    "tool_call",
    "tool_result",
    "tool_output_chunk",
    "subagent_child",
    "subagent_progress",
    "model_change",
    "cwd_change",
    "project_change",
    "permission_mode_change",
    "thinking_level_change",
    "plan_mode_change",
    "plan_update",
    "plan_review",
    "goal",
    "title",
    "browser_suites",
    "background_task",
    "approval",
    "pinned_archived",
    "compaction_started",
    "metrics",
    "custom",
    "active_tools_change",
];

fn entry_type_tag(entry: &manox_harness::session::SessionTreeEntry) -> String {
    serde_json::to_value(entry)
        .ok()
        .and_then(|value| {
            value
                .get("type")
                .and_then(|tag| tag.as_str())
                .map(str::to_string)
        })
        .unwrap_or_default()
}

/// K1 (L10) replay-consistency gate: a scripted session covering every
/// replay-supported journal kind — the live state (replay fold, restore
/// rebuild, display projection, cursor, transcript) must equal, field
/// by field, the state rebuilt from the on-disk file through the
/// production reload path (`builder.open`, the same seam `run_actor`'s
/// restore uses). Timestamps inside message payloads are the one
/// as-built exception (K5): the journal is the authority and they are
/// not a byte-for-byte assertion face, so the transcript comparison
/// strips them.
#[tokio::test]
async fn journal_replay_is_consistent_across_disk_reload() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("proj");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let sessions = dir.path().join("sessions");

    let stream = Arc::new(ToolRoundsStream {
        rounds: 0, // first call answers "done": the turn completes
        call: std::sync::atomic::AtomicUsize::new(0),
    });
    let resolver_for = |stream: Arc<ToolRoundsStream>| {
        let resolver: manox_harness::agent_loop::StreamResolver = Arc::new(move |_m: &PiModel| {
            Ok(Arc::clone(&stream) as Arc<dyn manox_harness::agent_loop::StreamFn>)
        });
        resolver
    };
    let mut session = create_agent_session()
        .with_cwd(&cwd)
        .with_session_dir(sessions.clone())
        .with_agent_dir(dir.path().join("agent"))
        .with_model_runtime(ModelRuntime::new(resolver_for(Arc::clone(&stream))))
        .with_model(test_model())
        .with_system_prompt("You are a test assistant.")
        .build()
        .await
        .unwrap();

    // A real turn: the user + assistant `message` entries land through
    // the persistence middleware (the deferred session materializes).
    session.prompt("replay consistency turn").await.unwrap();

    // One entry per supported kind through the production typed-append
    // face — the same `append_typed` the serializer runs.
    let appender = session.journal_appender();
    for (kind, payload) in scripted_state_rows() {
        appender
            .append_typed(kind, payload)
            .await
            .unwrap_or_else(|err| panic!("typed append `{kind}` must land: {err:#}"));
    }
    // The kernel-written faces: the reasoning-effort entry in its
    // on-disk vocabulary (the `set_thinking_level` clamp against a
    // non-thinking test model would record "off", which is not an
    // effort — the clamp is kernel-owned and harness-tested) and the
    // active-tool set through the production kernel face.
    appender
        .append_typed(
            "thinking_level_change",
            serde_json::json!({ "thinkingLevel": "max" }),
        )
        .await
        .unwrap();
    session
        .set_active_tools(vec!["Read".into(), "Grep".into()])
        .await
        .unwrap();
    // A UI annotation card (the display projection's `custom` face) —
    // appended through the same storage face persist_ui_note uses (the
    // resilient wrapper needs the actor's state/notice sink, which this
    // storage-level replay test does not run).
    let note_record = UiNoteRecord {
        kind: crate::db::UiNoteKind::Notice,
        data: serde_json::json!({ "text": "scripted note" }),
    };
    session
        .append_custom(UI_NOTE_CUSTOM_TYPE, serde_json::to_value(&note_record).ok())
        .await
        .unwrap();

    // ── Live-state capture. ──────────────────────────────────────────
    let live_records = appender.storage().journal_range(0, u64::MAX).await.unwrap();
    let live_replay = crate::replay::replay_thread_state(&live_records);
    let live_rebuilt = rebuild_restored_state(&session, &sessions).await;
    let live_entries = session.context_entries().await.unwrap();
    let (live_display, live_notes) = adapt::entries_to_display(&live_entries);
    let live_cursor = appender.storage().journal_cursor().await;
    let strip_timestamps = |messages: &[AgentMessage]| -> Vec<serde_json::Value> {
        messages
            .iter()
            .map(|message| {
                let mut value = serde_json::to_value(message).unwrap();
                if let Some(object) = value.as_object_mut() {
                    object.remove("timestamp");
                }
                value
            })
            .collect()
    };
    let live_transcript = strip_timestamps(session.harness_messages());
    let live_path = session.path().to_path_buf();

    // The fold reflects the scripted decisions (the entries are the
    // authority — not empty defaults), last-wins per field.
    assert_eq!(live_replay.title.as_deref(), Some("replayed title"));
    assert_eq!(live_replay.pinned, Some(false));
    assert_eq!(live_replay.archived, Some(true));
    assert_eq!(live_replay.project, Some(Some("/replay/proj".into())));
    assert_eq!(
        live_replay.permission_mode,
        Some(PermissionMode::DangerFullAccess)
    );
    assert_eq!(live_replay.reasoning_effort, Some(ReasoningEffort::Max));
    assert_eq!(live_replay.plan_mode, Some(true));
    assert_eq!(live_replay.cwd.as_deref(), Some("/replay/work"));
    assert_eq!(
        live_replay.goal,
        Some(serde_json::json!({ "objective": "replay" }))
    );
    assert_eq!(
        live_rebuilt.title.as_deref(),
        Some("replayed title"),
        "restore rebuild must read the journal title"
    );
    assert!(!live_rebuilt.pinned && live_rebuilt.archived);
    assert_eq!(live_rebuilt.project, Some(PathBuf::from("/replay/proj")));
    assert_eq!(
        live_rebuilt.permission_mode,
        PermissionMode::DangerFullAccess
    );
    assert_eq!(live_rebuilt.reasoning_effort, ReasoningEffort::Max);
    assert!(live_rebuilt.plan_mode);

    // K2 cache repair: the diverging (here: empty) sidecar converges
    // toward the journal authority — INCLUDING the title (the repair
    // is enabled by the rename-route decision: a pi thread's sidecar
    // title is written only by the journaled auto-title scheduler; the
    // direct writer `set_external_title` serves external TUI sessions,
    // which carry no pi chain and never reach this rebuild).
    let repaired = manox_harness::session_meta::load(&sessions, &live_path)
        .await
        .unwrap();
    assert_eq!(repaired.title.as_deref(), Some("replayed title"));
    assert!(!repaired.pinned && repaired.archived);
    assert_eq!(repaired.project.as_deref(), Some("/replay/proj"));
    assert_eq!(
        repaired.approval_mode.as_deref(),
        Some("danger-full-access")
    );
    assert_eq!(repaired.reasoning_effort.as_deref(), Some("max"));
    // Plan state has no sidecar mirror anymore (W4): the journal is the
    // single source, so there is nothing to repair or assert here.

    // Coverage guard: every supported kind is actually on the chain —
    // a silently skipped kind would make the round-trip assertions
    // vacuous.
    let live_tags: std::collections::HashSet<String> = live_records
        .iter()
        .map(|record| entry_type_tag(&record.entry))
        .collect();
    for kind in REPLAY_COVERAGE_KINDS {
        assert!(
            live_tags.contains(*kind),
            "the scripted session must cover `{kind}`; on chain: {live_tags:?}"
        );
    }

    // ── Reload through the production path and re-capture. ──────────
    session.close().await.unwrap();
    let reloaded = create_agent_session()
        .with_cwd(&cwd)
        .with_session_dir(sessions.clone())
        .with_agent_dir(dir.path().join("agent"))
        .with_model_runtime(ModelRuntime::new(resolver_for(Arc::clone(&stream))))
        .with_model(test_model())
        .with_system_prompt("You are a test assistant.")
        .open(live_path)
        .await
        .unwrap();
    let reloaded_records = reloaded.journal_range(0, u64::MAX).await.unwrap();
    let reloaded_replay = crate::replay::replay_thread_state(&reloaded_records);
    let reloaded_rebuilt = rebuild_restored_state(&reloaded, &sessions).await;
    let reloaded_entries = reloaded.context_entries().await.unwrap();
    let (reloaded_display, reloaded_notes) = adapt::entries_to_display(&reloaded_entries);
    let reloaded_cursor = reloaded.journal_cursor().await;
    let reloaded_transcript = strip_timestamps(reloaded.harness_messages());

    // ── Disk reload == live memory, field by field (§J.2). ──────────
    assert_eq!(
        live_replay, reloaded_replay,
        "the replay fold diverged across the disk round trip"
    );
    assert_eq!(
        live_rebuilt, reloaded_rebuilt,
        "the restore rebuild diverged across the disk round trip"
    );
    assert_eq!(
        live_records.len(),
        reloaded_records.len(),
        "the reloaded chain lost or gained entries"
    );
    for (live, reloaded) in live_records.iter().zip(&reloaded_records) {
        assert_eq!(live.seq, reloaded.seq, "seq diverged on the round trip");
        assert_eq!(
            serde_json::to_value(&live.entry).unwrap(),
            serde_json::to_value(&reloaded.entry).unwrap(),
            "entry `{}` diverged on the round trip",
            entry_type_tag(&live.entry)
        );
    }
    // The display comparison strips the per-message `id`: the mapping
    // mints a fresh UUID on every rebuild (it is not journal-derived),
    // so like message-payload timestamps (the K5 as-built note) it is
    // not a byte-for-byte assertion face. Everything else — roles,
    // content, note cards, ordering — must round-trip exactly. `timestamp`
    // is stripped for the same reason: the projection stamps wall-clock
    // time at rebuild, so the live and reloaded faces differ by whatever
    // seconds elapsed between them (the second-boundary flake this once
    // shipped as a CI-red).
    let display_shape = |display: &[HistoryEntry]| -> Vec<serde_json::Value> {
        display
            .iter()
            .map(|entry| {
                let mut value = serde_json::to_value(entry).unwrap();
                if let Some(message) = value.get_mut("Message").and_then(|m| m.as_object_mut()) {
                    message.remove("id");
                    message.remove("timestamp");
                }
                value
            })
            .collect()
    };
    assert_eq!(
        display_shape(&live_display),
        display_shape(&reloaded_display),
        "the display projection diverged across the disk round trip"
    );
    let note_shape = |notes: &[crate::db::PositionedNote]| -> Vec<serde_json::Value> {
        notes
            .iter()
            .map(|note| {
                serde_json::json!({
                    "afterMessage": note.after_message,
                    "note": serde_json::to_value(&note.note).unwrap(),
                })
            })
            .collect()
    };
    assert_eq!(
        note_shape(&live_notes),
        note_shape(&reloaded_notes),
        "the UI-note positions diverged across the disk round trip"
    );
    assert_eq!(
        live_cursor, reloaded_cursor,
        "the journal cursor diverged across the disk round trip"
    );
    assert_eq!(
        live_transcript, reloaded_transcript,
        "the transcript diverged across the disk round trip"
    );
    // The reloaded chain stays dense (L4): contiguous seq from 0 and
    // the cursor sits on the last record.
    for (index, record) in reloaded_records.iter().enumerate() {
        assert_eq!(record.seq, index as u64, "the reloaded chain is not dense");
    }
    assert_eq!(
        reloaded_cursor,
        reloaded_records
            .last()
            .map(|record| record.seq)
            .unwrap_or(0)
    );
}

/// K2 authority: the journal rebuild WINS over a diverging sidecar, and
/// the sidecar cache is repaired toward the journal.
#[tokio::test]
async fn restored_state_prefers_journal_over_sidecar_and_repairs_cache() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("proj");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let sessions = dir.path().join("sessions");
    let stream = Arc::new(ToolRoundsStream {
        rounds: 0,
        call: std::sync::atomic::AtomicUsize::new(0),
    });
    let resolver: manox_harness::agent_loop::StreamResolver = Arc::new(move |_m: &PiModel| {
        Ok(Arc::clone(&stream) as Arc<dyn manox_harness::agent_loop::StreamFn>)
    });
    let session = create_agent_session()
        .with_cwd(&cwd)
        .with_session_dir(sessions.clone())
        .with_agent_dir(dir.path().join("agent"))
        .with_model_runtime(ModelRuntime::new(resolver))
        .with_model(test_model())
        .with_system_prompt("You are a test assistant.")
        .build()
        .await
        .unwrap();

    // A stale sidecar: the cache says one thing …
    manox_harness::session_meta::update(&sessions, session.path(), |meta| {
        meta.title = Some("stale sidecar title".into());
        meta.pinned = true;
        meta.archived = false;
        meta.approval_mode = Some("read-only".into());
        meta.project = Some("/stale/project".into());
    })
    .await
    .unwrap();
    // … the journal says another (K3's decision-point entries).
    let appender = session.journal_appender();
    appender
        .append_typed("title", serde_json::json!({ "title": "journal title" }))
        .await
        .unwrap();
    appender
        .append_typed(
            "pinned_archived",
            serde_json::json!({ "pinned": false, "archived": true }),
        )
        .await
        .unwrap();
    appender
        .append_typed(
            "permission_mode_change",
            serde_json::json!({ "mode": "workspace-write" }),
        )
        .await
        .unwrap();
    appender
        .append_typed(
            "project_change",
            serde_json::json!({ "path": "/journal/project" }),
        )
        .await
        .unwrap();
    appender
        .append_typed(
            "goal",
            serde_json::json!({ "goal": { "objective": "restored goal" } }),
        )
        .await
        .unwrap();
    appender
        .append_typed(
            "plan_review",
            serde_json::json!({ "state": "proposed", "planFile": null }),
        )
        .await
        .unwrap();

    let rebuilt = rebuild_restored_state(&session, &sessions).await;
    assert_eq!(rebuilt.title.as_deref(), Some("journal title"));
    assert!(!rebuilt.pinned);
    assert!(rebuilt.archived);
    assert_eq!(rebuilt.permission_mode, PermissionMode::WorkspaceWrite);
    assert_eq!(rebuilt.project, Some(PathBuf::from("/journal/project")));
    // Goal stage ②: the rebuild passes the replayed journal snapshot
    // through to the Ready chain (the bridge seed's authority).
    assert_eq!(
        rebuilt
            .goal
            .as_ref()
            .and_then(|g| g.get("objective"))
            .and_then(|o| o.as_str()),
        Some("restored goal")
    );
    // Plan-review C4 vocabulary: the chain's proposed edge is the fold
    // source — the sidecar carries no flag, so a sidecar-only merge
    // would answer false.
    assert!(
        rebuilt.plan_review_pending,
        "the journal plan_review edge must fold the pending flag"
    );

    // The cache converged toward the authority in the same pass —
    // the title INCLUDED (the rename-route decision: the pi-thread
    // sidecar title's only writer is the journaled auto-title
    // scheduler, so a divergence is a stale cache, not a newer user
    // decision; external-session titles never reach this rebuild).
    let repaired = manox_harness::session_meta::load(&sessions, session.path())
        .await
        .unwrap();
    assert_eq!(repaired.title.as_deref(), Some("journal title"));
    assert!(!repaired.pinned && repaired.archived);
    assert_eq!(repaired.approval_mode.as_deref(), Some("workspace-write"));
    assert_eq!(repaired.project.as_deref(), Some("/journal/project"));
    session.close().await.unwrap();
}

/// K2 migration window: a legacy chain without decision-point entries
/// resolves from the sidecar for the fields that still cache there — and
/// plan state no longer does: with the `plan_mode_change` /
/// `plan_mode_request` / `plan_review` entries as the single source, a
/// chain that never saw them restores plan mode OFF (the radical
/// no-compat-read stance; W4 sidecar retirement).
#[tokio::test]
async fn restored_state_falls_back_to_sidecar_for_legacy_chains() {
    let replayed = crate::replay::ReplayedThreadState::default();
    let meta = manox_harness::session_meta::SessionMeta {
        title: Some("sidecar title".into()),
        project: Some("/sidecar/project".into()),
        approval_mode: Some("read-only".into()),
        reasoning_effort: Some("max".into()),
        plan_file: Some("/plans/x-plan.md".into()),
        plan_snapshot: Some(serde_json::json!([{ "content": "step" }])),
        pinned: true,
        archived: true,
        ..Default::default()
    };
    let merged = merge_restored_state(&replayed, &meta);
    assert_eq!(merged.title.as_deref(), Some("sidecar title"));
    assert_eq!(merged.project, Some(PathBuf::from("/sidecar/project")));
    assert_eq!(merged.permission_mode, PermissionMode::ReadOnly);
    assert!(!merged.plan_mode, "plan state is journal-only now");
    assert_eq!(merged.reasoning_effort, ReasoningEffort::Max);
    assert_eq!(merged.plan_file.as_deref(), Some("/plans/x-plan.md"));
    assert!(!merged.plan_review_pending);
    assert_eq!(
        merged.plan_snapshot,
        Some(serde_json::json!([{ "content": "step" }]))
    );
    assert!(merged.pinned && merged.archived);
}

/// A cleared plan (the empty `plan_update` snapshot the facade
/// persists on clear) normalizes to the sidecar's absence semantics —
/// the rebuild must not resurrect an empty plan rail.
#[test]
fn cleared_plan_snapshot_normalizes_to_absence() {
    let replayed = crate::replay::ReplayedThreadState {
        plan_snapshot: Some(serde_json::json!([])),
        ..Default::default()
    };
    let meta = manox_harness::session_meta::SessionMeta::default();
    let merged = merge_restored_state(&replayed, &meta);
    assert_eq!(merged.plan_snapshot, None);
}

/// K3 routing: a store-level decision row reaches the live actor's
/// command queue through the registry route, and the shutdown claim
/// lands queued rows in the journal even when they arrive behind the
/// actor's `Shutdown` break (the gateway archives right after dispose).
#[tokio::test]
async fn store_journal_rows_route_to_the_actor_and_the_shutdown_claim_lands_them() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("proj");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let thread_id = format!("route-test-{}", uuid::Uuid::new_v4());

    let stream = Arc::new(ToolRoundsStream {
        rounds: 0,
        call: std::sync::atomic::AtomicUsize::new(0),
    });
    let resolver: manox_harness::agent_loop::StreamResolver = Arc::new(move |_m: &PiModel| {
        Ok(Arc::clone(&stream) as Arc<dyn manox_harness::agent_loop::StreamFn>)
    });
    let session = create_agent_session()
        .with_cwd(&cwd)
        .with_session_dir(dir.path().join("sessions"))
        .with_agent_dir(dir.path().join("agent"))
        .with_model_runtime(ModelRuntime::new(resolver))
        .with_model(test_model())
        .with_system_prompt("You are a test assistant.")
        .build()
        .await
        .unwrap();

    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<SessionCmd>();
    register_engine_route(&thread_id, &cmd_tx);

    // Live route: the dispatch lands on the actor queue (send order =
    // persist order, §C.3).
    dispatch_store_journal_row(
        thread_id.clone(),
        None,
        "pinned_archived".into(),
        serde_json::json!({ "pinned": true, "archived": false }),
    );
    let queued = tokio::time::timeout(std::time::Duration::from_secs(5), cmd_rx.recv())
        .await
        .expect("the routed row must reach the actor queue")
        .expect("the channel stays open");
    match queued {
        SessionCmd::AppendJournal { kind, payload } => {
            assert_eq!(kind, "pinned_archived");
            assert_eq!(payload["pinned"], serde_json::json!(true));
        }
        _other => panic!("expected an AppendJournal row, got a different SessionCmd variant"),
    }

    // Shutdown claim: a row queued while the actor is exiting is
    // claimed under the registry lock and appended before close.
    let appender = session.journal_appender();
    dispatch_store_journal_row(
        thread_id.clone(),
        None,
        "pinned_archived".into(),
        serde_json::json!({ "pinned": false, "archived": true }),
    );
    let claimed = retire_and_claim_journal_rows(&thread_id, &mut cmd_rx);
    assert_eq!(claimed.len(), 1, "the queued row must be claimed");
    for (kind, payload) in claimed {
        append_row_fail_loud(&appender, kind, payload).await;
    }
    let rows = appender.storage().journal_range(0, u64::MAX).await.unwrap();
    assert!(
        rows.iter().any(|record| matches!(
            &record.entry,
            manox_harness::session::SessionTreeEntry::PinnedArchived { pinned, archived, .. }
                if !*pinned && *archived
        )),
        "the claimed row must land in the journal"
    );

    // A retired route never accepts new rows: the dispatch waits for
    // the route's removal, then takes the cold path (no file here —
    // the row is skipped, which the wait+removal must not hang on).
    unregister_engine_route(&thread_id, &cmd_tx);
    dispatch_store_journal_row(
        thread_id.clone(),
        None,
        "pinned_archived".into(),
        serde_json::json!({ "pinned": true, "archived": true }),
    );
    // Give the spawned waiter a beat; the assertion is that the route
    // table stays clean (the test's global-state hygiene) and nothing
    // panics or hangs.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        engine_routes().lock().unwrap().get(&thread_id).is_none(),
        "the test must leave the engine-route registry clean"
    );
    session.close().await.unwrap();
}

/// A minimal session for the K3 decision-point tests: a real jsonl
/// storage (deferred until first write), a scripted stream that is
/// never exercised, and the production builder path.
async fn decision_rig_session(dir: &tempfile::TempDir) -> AgentSession {
    let cwd = dir.path().join("proj");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let stream = Arc::new(ToolRoundsStream {
        rounds: 0,
        call: std::sync::atomic::AtomicUsize::new(0),
    });
    let resolver: manox_harness::agent_loop::StreamResolver = Arc::new(move |_m: &PiModel| {
        Ok(Arc::clone(&stream) as Arc<dyn manox_harness::agent_loop::StreamFn>)
    });
    create_agent_session()
        .with_cwd(&cwd)
        .with_session_dir(dir.path().join("sessions"))
        .with_agent_dir(dir.path().join("agent"))
        .with_model_runtime(ModelRuntime::new(resolver))
        .with_model(test_model())
        .with_system_prompt("You are a test assistant.")
        .build()
        .await
        .unwrap()
}

/// K3 (L3) regression: the project-binding decision journals a
/// `project_change` entry alongside the sidecar cache write — pre-fix
/// the binding was written to the sidecar only, leaving the chain
/// without the authority K2 rebuilds from.
#[tokio::test]
async fn project_binding_journals_project_change_entry() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let session = decision_rig_session(&dir).await;
    let state = test_engine_state();
    let (notice_tx, _notice_rx) = mpsc::unbounded_channel::<BackendNotice>();

    bind_project(
        &sessions,
        &session,
        Path::new("/bound/project"),
        &state,
        &notice_tx,
    )
    .await;

    let rows = session.journal_range(0, u64::MAX).await.unwrap();
    assert!(
        rows.iter().any(|record| matches!(
            &record.entry,
            manox_harness::session::SessionTreeEntry::ProjectChange { path, .. }
                if path.as_deref() == Some("/bound/project")
        )),
        "the binding must journal a project_change entry; chain kinds: {:?}",
        rows.iter()
            .map(|record| entry_type_tag(&record.entry))
            .collect::<Vec<_>>()
    );
    // The sidecar cache follows (fast-list mirror, K2).
    let meta = manox_harness::session_meta::load(&sessions, session.path())
        .await
        .unwrap();
    assert_eq!(meta.project.as_deref(), Some("/bound/project"));
    session.close().await.unwrap();
}

/// K3 (L3) regression: the permission-mode decision emits the notice
/// whose tap mapping journals `permission_mode_change` — pre-fix the
/// choice landed in the gate and the sidecar only, so the chain never
/// carried the user's mode toggle.
#[tokio::test]
async fn permission_mode_decision_journals_through_the_tap() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let session = decision_rig_session(&dir).await;
    let state = test_engine_state();
    let (notice_tx, mut notice_rx) = mpsc::unbounded_channel::<BackendNotice>();

    apply_permission_mode(
        &state,
        &sessions,
        session.path(),
        PermissionMode::ReadOnly,
        &notice_tx,
    )
    .await;

    assert_eq!(state.gate.mode(), PermissionMode::ReadOnly);
    let meta = manox_harness::session_meta::load(&sessions, session.path())
        .await
        .unwrap();
    assert_eq!(meta.approval_mode.as_deref(), Some("read-only"));

    // The decision notifies, and the tap's mapping of that notice is
    // the journal row (the same `durable_journal_payload` face the
    // spawn_engine tap runs). Bounded: a missing emission must fail
    // the test, not hang it.
    let notice = tokio::time::timeout(std::time::Duration::from_secs(5), notice_rx.recv())
        .await
        .unwrap_or_else(|_| panic!("the mode decision must reach the notice face"))
        .expect("the notice channel must stay open");
    let BackendNotice::Event(event) = notice else {
        panic!("the mode decision must ride a ThreadEvent notice");
    };
    let ThreadEvent::PermissionModeChanged { mode } = *event else {
        panic!("expected PermissionModeChanged");
    };
    assert_eq!(mode, PermissionMode::ReadOnly);
    let (kind, payload) = durable_journal_payload(&ThreadEvent::PermissionModeChanged { mode })
        .expect("the tap maps the decision to a typed row");
    assert_eq!(kind, "permission_mode_change");
    session
        .journal_appender()
        .append_typed(&kind, payload)
        .await
        .unwrap();
    let rows = session.journal_range(0, u64::MAX).await.unwrap();
    assert!(
        rows.iter().any(|record| matches!(
            &record.entry,
            manox_harness::session::SessionTreeEntry::PermissionModeChange { mode, .. }
                if mode == "read-only"
        )),
        "the decision must land a permission_mode_change entry"
    );
    session.close().await.unwrap();
}

/// A fresh session chain establishment journals exactly one
/// `cwd_change` witness for the effective working directory (the
/// follow stream's projection fold never sees the session file's
/// header), plus one `CwdChanged` on the notice face.
#[tokio::test]
async fn establishment_journals_one_cwd_witness() {
    let dir = tempfile::tempdir().unwrap();
    let session = decision_rig_session(&dir).await;
    let state = test_engine_state();
    let (notice_tx, mut notice_rx) = mpsc::unbounded_channel::<BackendNotice>();

    announce_established_cwd(&session, &state, &notice_tx).await;

    let projected_str = session.projected_cwd().await.to_string_lossy().into_owned();
    let rows = session.journal_range(0, u64::MAX).await.unwrap();
    let witnesses = rows
        .iter()
        .filter(|record| {
            matches!(
                &record.entry,
                manox_harness::session::SessionTreeEntry::CwdChange { cwd, .. }
                    if *cwd == projected_str
            )
        })
        .count();
    assert_eq!(
        witnesses,
        1,
        "establishment journals exactly one cwd_change witness; chain kinds: {:?}",
        rows.iter()
            .map(|record| entry_type_tag(&record.entry))
            .collect::<Vec<_>>()
    );
    let notice = tokio::time::timeout(std::time::Duration::from_secs(5), notice_rx.recv())
        .await
        .unwrap_or_else(|_| panic!("the establishment must reach the notice face"))
        .expect("the notice channel must stay open");
    let BackendNotice::Event(event) = notice else {
        panic!("the establishment must ride a ThreadEvent notice");
    };
    let ThreadEvent::CwdChanged { path } = *event else {
        panic!("expected CwdChanged");
    };
    assert_eq!(path, projected_str);
    session.close().await.unwrap();
}

/// The working-directory switch onto the already-projected tail is
/// witness-free (the establishment announcement stays the chain's
/// single `cwd_change`); a switch onto a different directory lands its
/// own durable move.
#[tokio::test]
async fn bind_order_leaves_exactly_one_cwd_witness_on_the_new_chain() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = decision_rig_session(&dir).await;
    let state = test_engine_state();
    let (notice_tx, _notice_rx) = mpsc::unbounded_channel::<BackendNotice>();

    let project = session.projected_cwd().await;
    announce_established_cwd(&session, &state, &notice_tx).await;
    handle_set_cwd(&mut session, &state, &notice_tx, &project).await;

    let rows = session.journal_range(0, u64::MAX).await.unwrap();
    let witnesses = rows
        .iter()
        .filter(|record| {
            matches!(
                &record.entry,
                manox_harness::session::SessionTreeEntry::CwdChange { .. }
            )
        })
        .count();
    assert_eq!(
        witnesses, 1,
        "the no-op switch must not stack atop the establishment witness"
    );

    let other = dir.path().to_path_buf();
    handle_set_cwd(&mut session, &state, &notice_tx, &other).await;
    let rows = session.journal_range(0, u64::MAX).await.unwrap();
    let tail = rows.iter().rev().find_map(|record| match &record.entry {
        manox_harness::session::SessionTreeEntry::CwdChange { cwd, .. } => Some(cwd.clone()),
        _ => None,
    });
    assert_eq!(
        tail.as_deref(),
        Some(other.to_string_lossy().as_ref()),
        "a switch onto a different directory still lands its own durable move"
    );
    session.close().await.unwrap();
}

/// Hot-switch contract: `set_permission_mode` lands the mode on the live
/// gate before the actor ever drains its queue, so a mid-turn switch
/// governs the very next tool call (every resolver reads the gate per
/// call). The queued command below still carries the durable half.
#[tokio::test]
async fn set_permission_mode_writes_the_gate_before_the_actor_drains() {
    let state = test_engine_state();
    assert_eq!(state.gate.mode(), PermissionMode::default());
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<SessionCmd>();
    let (notice_tx, _notice_rx) = mpsc::unbounded_channel::<BackendNotice>();
    let bus = crate::steer_bus::AgentBus::new("test-thread".into(), notice_tx);
    let engine = PiEngine {
        cmd_tx,
        state: Arc::clone(&state),
        bus,
    };

    ThreadEngine::set_permission_mode(&engine, PermissionMode::ReadOnly);

    // The gate flipped with no actor running at all.
    assert_eq!(state.gate.mode(), PermissionMode::ReadOnly);
    // The durable command is still queued, in order, with the same value.
    match cmd_rx.try_recv() {
        Ok(SessionCmd::SetPermissionMode(mode)) => {
            assert_eq!(mode, PermissionMode::ReadOnly)
        }
        Ok(_) => panic!("a different command rode the queue for one switch"),
        Err(_) => panic!("the switch must still queue its durable command"),
    }
    assert!(
        cmd_rx.try_recv().is_err(),
        "nothing else may ride the queue for one switch"
    );
}

/// K3 (L3) regression: the initial-title decision rides the same
/// notice tap as the generated title — pre-fix only
/// `SessionListDirty` fired, so the chain never carried a `title`
/// entry for the initial title and the K2 rebuild stayed blind to it.
#[tokio::test]
async fn initial_title_decision_journals_through_the_tap() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let session = decision_rig_session(&dir).await;
    let (notice_tx, mut notice_rx) = mpsc::unbounded_channel::<BackendNotice>();

    persist_initial_title(
        &sessions,
        session.path(),
        "initial title".to_string(),
        &notice_tx,
    )
    .await;

    let meta = manox_harness::session_meta::load(&sessions, session.path())
        .await
        .unwrap();
    assert_eq!(meta.title.as_deref(), Some("initial title"));

    // First the sidebar refresh, then the TitleChanged notice whose
    // tap mapping is the `title` row. Bounded: a missing emission must
    // fail the test, not hang it.
    let first = tokio::time::timeout(std::time::Duration::from_secs(5), notice_rx.recv())
        .await
        .unwrap_or_else(|_| panic!("the sidebar must refresh"))
        .expect("the notice channel must stay open");
    assert!(
        matches!(first, BackendNotice::SessionListDirty),
        "the refresh notice comes first"
    );
    let second = tokio::time::timeout(std::time::Duration::from_secs(5), notice_rx.recv())
        .await
        .unwrap_or_else(|_| panic!("the title decision must reach the notice face"))
        .expect("the notice channel must stay open");
    let BackendNotice::Event(event) = second else {
        panic!("the title decision must ride a ThreadEvent notice");
    };
    let ThreadEvent::TitleChanged { title } = *event else {
        panic!("expected TitleChanged");
    };
    assert_eq!(title, "initial title");
    let (kind, payload) = durable_journal_payload(&ThreadEvent::TitleChanged { title })
        .expect("the tap maps the decision to a typed row");
    assert_eq!(kind, "title");
    session
        .journal_appender()
        .append_typed(&kind, payload)
        .await
        .unwrap();
    let rows = session.journal_range(0, u64::MAX).await.unwrap();
    assert!(
        rows.iter().any(|record| matches!(
            &record.entry,
            manox_harness::session::SessionTreeEntry::Title { title, .. }
                if title == "initial title"
        )),
        "the decision must land a title entry"
    );
    session.close().await.unwrap();
}

// ── #805 companion: embedder tools registered AFTER session assembly
// must reach the live tool table. The VS Code host registers only once
// it learns the session id — i.e. after Open/Create spawned the engine
// — so the one-time `build_tools` snapshot is stale by construction:
// the adapters reach neither the model's schema nor `execute_one`'s
// dispatch table, and the model's call fails with
// `Tool not found: client_<name>`. These tests pin the per-Prompt
// refresh (`refresh_embedder_tools`, the call the actor makes before
// every run) at the exact breakage point: a real AgentSession, the
// real process provider slot, and a stub model that streams a
// `tool_use` for the registered name — dispatch must reach the
// adapter (the in-engine stand-in for the server's `InvokeClientTool`
// round trip) instead of the not-found error. The register-before-
// assembly order stays covered by `build_tools` itself (plus the
// napi-edge test in manox-napi); register-after is the shipping VS
// Code flow.

/// Guard for the process-wide provider slot across the tests here
/// (mirrors session-core's `lock_globals`; the slot is last-wins).
static EMBEDDER_SLOT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A client tool that records its own executions — the in-engine
/// stand-in for the `EmbedderToolAdapter`'s `InvokeClientTool` call.
#[derive(Clone)]
struct ProbeClientTool {
    invocations: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl manox_harness::tool::AgentTool for ProbeClientTool {
    fn name(&self) -> &str {
        "client_probe"
    }
    fn description(&self) -> &str {
        "registered after the session was assembled"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    // The GenCodeChain registration is `read_only: true`; a gated
    // probe would park the run on an approval card.
    fn is_read_only(&self) -> bool {
        true
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        _params: serde_json::Value,
        _signal: tokio_util::sync::CancellationToken,
        _ctx: &dyn manox_harness::tool::ToolContext,
    ) -> Result<manox_harness::tool::AgentToolResult, manox_harness::tool::ToolError> {
        self.invocations
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(manox_harness::tool::AgentToolResult::text("probe-ok"))
    }
}

/// Provider standing in for `AgentServerEmbedderTools`: hands out the
/// probe tool for one session id only once the test "registers" it —
/// mirroring `RegisterSessionTools` landing into the server map.
struct ProbeProvider {
    registered: Arc<std::sync::atomic::AtomicBool>,
    tool: ProbeClientTool,
    session_id: &'static str,
}

impl crate::embedder_tools::EmbedderToolProvider for ProbeProvider {
    fn tools_for(&self, session_id: &str) -> Vec<Arc<dyn manox_harness::tool::AgentTool>> {
        if self.registered.load(std::sync::atomic::Ordering::SeqCst)
            && session_id == self.session_id
        {
            vec![Arc::new(self.tool.clone()) as Arc<dyn manox_harness::tool::AgentTool>]
        } else {
            Vec::new()
        }
    }
}

/// Scripted provider: round 1 emits a `client_probe` tool_use (and
/// records whether the mounted schema advertised the tool); round 2
/// settles with plain text.
struct ProbeRoundStream {
    calls: std::sync::atomic::AtomicUsize,
    advertised: Arc<std::sync::atomic::AtomicBool>,
}

fn probe_tool_use(context: &manox_harness::types::AgentContext) -> AgentMessage {
    AgentMessage::Assistant {
        content: vec![ContentBlock::ToolUse {
            id: "probe-1".into(),
            name: "client_probe".into(),
            input: serde_json::json!({}),
            thought_signature: None,
        }],
        model: context.model.id.clone(),
        provider: context.model.provider.clone(),
        api: context.model.api.clone(),
        response_model: None,
        response_id: None,
        diagnostics: None,
        raw_stop_reason: None,
        stop_reason: Some(manox_harness::types::StopReason::ToolUse),
        usage: Box::new(manox_harness::types::Usage {
            input_tokens: 10,
            output_tokens: 5,
            ..Default::default()
        }),
        error_message: None,
        timestamp: chrono::Utc::now(),
    }
}

#[async_trait::async_trait]
impl manox_harness::agent_loop::StreamFn for ProbeRoundStream {
    async fn stream(
        &self,
        context: &manox_harness::types::AgentContext,
        _signal: tokio_util::sync::CancellationToken,
        _event_tx: tokio::sync::mpsc::Sender<manox_harness::types::AgentEvent>,
    ) -> Result<AgentMessage, anyhow::Error> {
        let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if n == 0 {
            if context.tools.iter().any(|t| t.name() == "client_probe") {
                self.advertised
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
            return Ok(probe_tool_use(context));
        }
        Ok(text_assistant(context, "settled"))
    }
}

/// Register-after-assembly, full end to end: the provider slot exists
/// but returns NOTHING at session build (the map is empty when the
/// engine spawns), the registration lands afterwards, and the refresh
/// must surface the tool in the model's schema AND dispatch the
/// stub's tool_use into the adapter (not `Tool not found`).
// The std guard only serializes tests against each other on the
// process-wide provider slot; a current-thread test runtime has no
// re-entrant taker, so holding it across the test's own awaits is safe.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn embedder_tool_registered_after_assembly_reaches_schema_and_dispatch() {
    let _slot = EMBEDDER_SLOT_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("proj");
    tokio::fs::create_dir_all(&cwd).await.unwrap();

    let registered = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let invocations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let advertised = Arc::new(std::sync::atomic::AtomicBool::new(false));
    crate::embedder_tools::set_provider(Arc::new(ProbeProvider {
        registered: Arc::clone(&registered),
        tool: ProbeClientTool {
            invocations: Arc::clone(&invocations),
        },
        session_id: "embedder-after",
    }));

    let stream = Arc::new(ProbeRoundStream {
        calls: std::sync::atomic::AtomicUsize::new(0),
        advertised: Arc::clone(&advertised),
    });
    let stream_for_resolver = Arc::clone(&stream);
    let resolver: manox_harness::agent_loop::StreamResolver = Arc::new(move |_m: &PiModel| {
        Ok(Arc::clone(&stream_for_resolver) as Arc<dyn manox_harness::agent_loop::StreamFn>)
    });
    let mut session = create_agent_session()
        .with_cwd(&cwd)
        .with_session_dir(dir.path().join("sessions"))
        .with_agent_dir(dir.path().join("agent"))
        .with_model_runtime(ModelRuntime::new(resolver))
        .with_model(test_model())
        .with_system_prompt("You are a test assistant.")
        .build()
        .await
        .unwrap();
    // The buggy state, pinned: at assembly the registration has not
    // landed, so the mounted table carries no client adapter — the
    // model never sees it and its call fails with `Tool not found`.
    assert!(
        !session.tools().iter().any(|n| n == "client_probe"),
        "precondition: assembly must NOT contain the late registration"
    );

    let state = test_engine_state();
    // The host registers — the `RegisterSessionTools` write landing
    // into the AgentServer map after the engine spawned.
    registered.store(true, std::sync::atomic::Ordering::SeqCst);

    // What the actor does at the start of every `Prompt`.
    refresh_embedder_tools(&mut session, "embedder-after", &state.gate).await;
    assert!(
        session.tools().iter().any(|n| n == "client_probe"),
        "refresh must surface the late registration in the tool table: {:?}",
        session.tools()
    );

    // And the round trip proves dispatch (not just the schema): the
    // stub's tool_use must land in the adapter, not in the
    // `Tool not found` arm of `execute_one`.
    let messages = session
        .prompt("use the host tool")
        .await
        .expect("the stub run completes");
    assert!(
        advertised.load(std::sync::atomic::Ordering::SeqCst),
        "the provider request schema must advertise client_probe"
    );
    assert_eq!(
        invocations.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the tool_use must dispatch into the registered adapter"
    );
    let serialized = serde_json::to_string(&messages).unwrap();
    assert!(
        !serialized.contains("Tool not found"),
        "dispatch must not answer with the not-found error: {serialized}"
    );
    assert!(serialized.contains("probe-ok"));

    crate::embedder_tools::drop_provider_for_test();
}

/// The narrowed-selection timing variant: a session whose active set
/// was narrowed (browser-suite-toggle shape) BEFORE the registration
/// lands must still dispatch the late tool — the refresh must widen
/// the selection, since `apply_active_tools` would otherwise filter
/// the freshly mounted adapter out again (schema present, dispatch
/// still `Tool not found` — the exact split the bug report showed).
// The std guard only serializes tests against each other on the
// process-wide provider slot; a current-thread test runtime has no
// re-entrant taker, so holding it across the test's own awaits is safe.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn late_registration_lands_in_the_active_selection_of_a_narrowed_session() {
    let _slot = EMBEDDER_SLOT_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("proj");
    tokio::fs::create_dir_all(&cwd).await.unwrap();

    let registered = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let invocations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let advertised = Arc::new(std::sync::atomic::AtomicBool::new(false));
    crate::embedder_tools::set_provider(Arc::new(ProbeProvider {
        registered: Arc::clone(&registered),
        tool: ProbeClientTool {
            invocations: Arc::clone(&invocations),
        },
        session_id: "embedder-narrowed",
    }));

    let stream = Arc::new(ProbeRoundStream {
        calls: std::sync::atomic::AtomicUsize::new(0),
        advertised: Arc::clone(&advertised),
    });
    let stream_for_resolver = Arc::clone(&stream);
    let resolver: manox_harness::agent_loop::StreamResolver = Arc::new(move |_m: &PiModel| {
        Ok(Arc::clone(&stream_for_resolver) as Arc<dyn manox_harness::agent_loop::StreamFn>)
    });
    let mut session = create_agent_session()
        .with_cwd(&cwd)
        .with_session_dir(dir.path().join("sessions"))
        .with_agent_dir(dir.path().join("agent"))
        .with_model_runtime(ModelRuntime::new(resolver))
        .with_model(test_model())
        .with_system_prompt("You are a test assistant.")
        .build()
        .await
        .unwrap();
    // Narrowed selection with no client tool in it.
    session.set_active_tools(vec![]).await.unwrap();
    registered.store(true, std::sync::atomic::Ordering::SeqCst);

    let state = test_engine_state();
    refresh_embedder_tools(&mut session, "embedder-narrowed", &state.gate).await;

    assert!(
        session.tools().iter().any(|n| n == "client_probe"),
        "the adapter must be mounted: {:?}",
        session.tools()
    );
    let active = session.active_tool_names().expect("selection stays Some");
    assert!(
        active.iter().any(|n| n == "client_probe"),
        "the narrowed selection must widen to the fresh registration: {active:?}"
    );

    let messages = session
        .prompt("use the host tool")
        .await
        .expect("the stub run completes");
    assert!(
        advertised.load(std::sync::atomic::Ordering::SeqCst),
        "the provider request schema must advertise client_probe"
    );
    assert_eq!(
        invocations.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the tool_use must dispatch into the registered adapter"
    );
    let serialized = serde_json::to_string(&messages).unwrap();
    assert!(
        !serialized.contains("Tool not found"),
        "dispatch must not answer with the not-found error: {serialized}"
    );

    crate::embedder_tools::drop_provider_for_test();
}
