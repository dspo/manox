//! Fold tests: a seeded journal on disk must reach the AHP chat/session
//! states the same host-and-client fold produces (§C, L10).

use ahp_types::state::{ResponsePart, ToolCallState, ToolResultContent, TurnState};
use ahp_types::state::{SessionLifecycle, SessionStatus};
use chrono::{DateTime, Utc};
use manox_ahp_runtime::ahp::{chat_state, session_state};
use manox_harness::session::SessionTreeEntry as E;
use manox_harness::session::jsonl::{JsonlSessionMetadata, JsonlSessionStorage};
use manox_harness::types::{AgentMessage, ContentBlock, Usage};
use serde_json::json;
use std::sync::Arc;

/// A fixed stamp so the folded `modifiedAt` is assertable (a pure fold
/// must not observe wall-clock time).
fn stamp() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2025-01-02T03:04:05.123Z")
        .unwrap()
        .with_timezone(&Utc)
}

fn wire_stamp() -> String {
    stamp().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Install the hermetic globals + a fresh standalone store, and point the
/// thread registry at a scratch file. Serialized by the suite's global
/// locks, exactly like the `journal_query` cold-read tests; the returned
/// guards keep the store override and the registry path effective for the
/// duration of the test.
pub(super) fn install() -> (
    std::sync::MutexGuard<'static, ()>,
    std::sync::MutexGuard<'static, ()>,
) {
    let outer = crate::test_support::lock_globals();
    let store_lock = manox_agent::thread_store::store_test_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    crate::test_support::hermetic_home();
    crate::test_support::init_globals();
    let tmp = std::env::temp_dir().join(format!(
        "manox-ahp-fold-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&tmp).unwrap();
    let db = Arc::new(manox_agent::db::ThreadsDatabase::open(&tmp.join("threads.db")).unwrap());
    manox_agent::thread_store::init_for_test(db);
    manox_agent::thread_registry::set_registry_path_for_test(Some(
        tmp.join("threads.registry.json"),
    ));
    std::fs::create_dir_all(manox_agent::thread_store::global_sessions_dir()).unwrap();
    (outer, store_lock)
}

/// Tear the installed overrides back down: suites outside this module may
/// run expecting "no store" (the `journal_query` fallback cold read), so a
/// leaked `TEST_OVERRIDE` is cross-suite red, not just local noise.
pub(super) fn uninstall() {
    manox_agent::thread_registry::set_registry_path_for_test(None);
    manox_agent::thread_store::drop_for_test();
}

/// Rebuild one sample entry with the chain id/parent the seed needs. The
/// sample constructors below pass placeholders; only id/parent matter to
/// the fold (all samples share the fixed [`stamp`]).
fn with_chain(id: &str, parent_id: Option<String>, event: E) -> E {
    let mut e = event;
    match &mut e {
        E::Message {
            id: i,
            parent_id: p,
            ..
        }
        | E::TurnStart {
            id: i,
            parent_id: p,
            ..
        }
        | E::TurnFinish {
            id: i,
            parent_id: p,
            ..
        }
        | E::AgentTextDelta {
            id: i,
            parent_id: p,
            ..
        }
        | E::ToolCall {
            id: i,
            parent_id: p,
            ..
        }
        | E::ToolResult {
            id: i,
            parent_id: p,
            ..
        }
        | E::Title {
            id: i,
            parent_id: p,
            ..
        }
        | E::ModelChange {
            id: i,
            parent_id: p,
            ..
        }
        | E::CwdChange {
            id: i,
            parent_id: p,
            ..
        }
        | E::PinnedArchived {
            id: i,
            parent_id: p,
            ..
        }
        | E::PlanModeChange {
            id: i,
            parent_id: p,
            ..
        } => {
            *i = id.to_string();
            *p = parent_id;
        }
        other => panic!("unrechained sample variant: {other:?}"),
    }
    e
}

/// Write one session journal (`<id>.jsonl` under the store's sessions dir,
/// header stamped with its thread) with a linear entry chain.
async fn seed_session(id: &str, thread: &str, cwd: &str, events: Vec<(&str, E)>) {
    let path = manox_agent::thread_store::global_sessions_dir().join(format!("{id}.jsonl"));
    let mut entries = Vec::with_capacity(events.len());
    let mut parent: Option<String> = None;
    for (eid, event) in events {
        entries.push(with_chain(eid, parent.take(), event));
        parent = Some(eid.to_string());
    }
    let storage = JsonlSessionStorage::create(
        &path,
        JsonlSessionMetadata {
            id: id.into(),
            cwd: cwd.into(),
            created_at: stamp(),
            parent_session_path: None,
            metadata: Some(json!({ "thread": thread })),
        },
    )
    .await
    .unwrap();
    storage.append_entries(&entries).await.unwrap();
}

// ── sample entry constructors (chain fields filled by `with_chain`) ─────

fn user(text: &str) -> E {
    E::Message {
        id: String::new(),
        parent_id: None,
        timestamp: stamp(),
        message: AgentMessage::User {
            content: vec![ContentBlock::Text {
                text: text.into(),
                signature: None,
            }],
            timestamp: stamp(),
            id: None,
        },
        origin: Some("rpc-1".into()),
    }
}

fn assistant(text: &str, input: u64, output: u64) -> E {
    E::Message {
        id: String::new(),
        parent_id: None,
        timestamp: stamp(),
        message: AgentMessage::Assistant {
            content: vec![ContentBlock::Text {
                text: text.into(),
                signature: None,
            }],
            model: "x-model".into(),
            provider: "newprov".into(),
            api: "anthropic".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            stop_reason: None,
            raw_stop_reason: None,
            usage: Box::new(Usage {
                input_tokens: input,
                output_tokens: output,
                ..Default::default()
            }),
            error_message: None,
            timestamp: stamp(),
        },
        origin: None,
    }
}

fn turn_start() -> E {
    E::TurnStart {
        id: String::new(),
        parent_id: None,
        timestamp: stamp(),
    }
}

fn turn_finish() -> E {
    E::TurnFinish {
        id: String::new(),
        parent_id: None,
        timestamp: stamp(),
        cancelled: false,
        failed: false,
        stranded_steer_ids: Vec::new(),
    }
}

fn delta(text: &str) -> E {
    E::AgentTextDelta {
        id: String::new(),
        parent_id: None,
        timestamp: stamp(),
        delta: text.into(),
    }
}

fn tool_call() -> E {
    E::ToolCall {
        id: String::new(),
        parent_id: None,
        timestamp: stamp(),
        call_id: "c1".into(),
        name: "bash".into(),
        title: "run ls".into(),
        status: "running".into(),
        input: Some(json!({ "command": "ls" })),
    }
}

fn tool_result() -> E {
    E::ToolResult {
        id: String::new(),
        parent_id: None,
        timestamp: stamp(),
        call_id: "c1".into(),
        output: "ok".into(),
        is_error: false,
    }
}

/// The transcript used by both fold tests: two turns, the first streamed
/// (deltas + a settled tool call), the second settled-only (no deltas).
fn transcript() -> Vec<(&'static str, E)> {
    vec![
        ("e1", user("hello")),
        ("e2", turn_start()),
        ("e3", delta("Hi ")),
        ("e4", delta("there")),
        ("e5", tool_call()),
        ("e6", tool_result()),
        ("e7", assistant("all done", 10, 5)),
        ("e8", turn_finish()),
        ("e9", turn_start()),
        ("e10", assistant("bye", 1, 2)),
        ("e11", turn_finish()),
    ]
}

/// Part kinds in stream order, for shape assertions.
fn kinds(parts: &[ResponsePart]) -> Vec<&'static str> {
    parts
        .iter()
        .map(|p| match p {
            ResponsePart::Markdown(_) => "markdown",
            ResponsePart::Reasoning(_) => "reasoning",
            ResponsePart::ToolCall(_) => "toolCall",
            ResponsePart::SystemNotification(_) => "systemNotification",
            _ => "other",
        })
        .collect()
}

#[test]
fn chat_fold_replays_the_journal_transcript() {
    let _guards = install();
    manox_agent::runtime::handle().block_on(async {
        seed_session("chatfold-1", "thread-A", "/", transcript()).await;
        let state = chat_state("chatfold-1").await.expect("chat state");

        assert_eq!(state.resource, manox_ahp::channels::chat::uri("chatfold-1"));
        assert!(state.active_turn.is_none(), "the journal closed both turns");
        assert!(state.queued_messages.as_ref().is_none_or(|q| q.is_empty()));
        // Determinism: the stamp is the journal's last entry, not "now".
        assert_eq!(state.modified_at, wire_stamp());

        assert_eq!(state.turns.len(), 2);
        let first = &state.turns[0];
        assert_eq!(first.message.text, "hello");
        assert_eq!(first.state, TurnState::Complete);
        // Deltas opened one markdown run; the tool call closed it; the
        // settled assistant row is silent (first-writer-wins on streamed
        // text), so the turn carries exactly [markdown, toolCall].
        assert_eq!(kinds(&first.response_parts), vec!["markdown", "toolCall"]);
        let ResponsePart::Markdown(md) = &first.response_parts[0] else {
            panic!("markdown first");
        };
        assert_eq!(md.content, "Hi there");
        let ResponsePart::ToolCall(call) = &first.response_parts[1] else {
            panic!("tool call second");
        };
        match &call.tool_call {
            ToolCallState::Completed(done) => {
                assert_eq!(done.tool_call_id, "c1");
                assert!(done.success, "the tool result reported success");
                let content = done.content.as_ref().expect("result content");
                assert!(
                    content.iter().any(|c| matches!(
                        c,
                        ToolResultContent::Text(t) if t.text == "ok"
                    )),
                    "the tool result text reaches the completed call: {content:?}"
                );
            }
            other => panic!("expected a completed tool call, got {other:?}"),
        }
        // Usage rides the turn the assistant row settled.
        let usage = first.usage.as_ref().expect("turn usage");
        assert_eq!(usage.input_tokens, Some(10));
        assert_eq!(usage.output_tokens, Some(5));

        let second = &state.turns[1];
        // A turn with no user row carries the empty `noUserRow` message.
        assert_eq!(second.message.text, "");
        assert_eq!(kinds(&second.response_parts), vec!["markdown"]);
        let ResponsePart::Markdown(md) = &second.response_parts[0] else {
            panic!("markdown");
        };
        assert_eq!(md.content, "bye");
    });
    uninstall();
}

#[test]
fn session_fold_merges_thread_metadata_and_journal_facts() {
    let _guards = install();
    manox_agent::runtime::handle().block_on(async {
        let store = manox_agent::thread_store::global();
        store.with_mut(|s| {
            s.insert_summary_for_test("thread-A", None);
            s.mark_running("thread-A");
        });
        manox_agent::thread_registry::set_active("thread-A", "chatfold-1").await;
        seed_session("chatfold-1", "thread-A", "/tmp/seed", transcript()).await;
        // Session-channel facts arrive on a sibling journal of the thread…
        seed_session(
            "chatfold-2",
            "thread-A",
            "/",
            vec![
                (
                    "f1",
                    E::Title {
                        id: String::new(),
                        parent_id: None,
                        timestamp: stamp(),
                        title: "Renamed thread".into(),
                    },
                ),
                (
                    "f2",
                    E::ModelChange {
                        id: String::new(),
                        parent_id: None,
                        timestamp: stamp(),
                        provider: "newprov".into(),
                        model_id: "x-model".into(),
                    },
                ),
                (
                    "f3",
                    E::CwdChange {
                        id: String::new(),
                        parent_id: None,
                        timestamp: stamp(),
                        cwd: "/work/x".into(),
                    },
                ),
                (
                    "f4",
                    E::PinnedArchived {
                        id: String::new(),
                        parent_id: None,
                        timestamp: stamp(),
                        pinned: false,
                        archived: true,
                    },
                ),
            ],
        )
        .await;
        // …and a foreign thread's journal must stay out of the catalogue.
        seed_session("other-sess", "thread-B", "/", transcript()).await;

        let state = session_state("thread-A").await.expect("session state");
        assert_eq!(
            state.title, "Renamed thread",
            "the journalled title beats the store mirror"
        );
        assert_eq!(state.provider, "newprov", "derived from the model ref");
        assert_eq!(state.lifecycle, SessionLifecycle::Ready);
        let active_chat = manox_ahp::channels::chat::uri("chatfold-1");
        assert_eq!(
            state.default_chat.as_deref(),
            Some(active_chat.as_str()),
            "the registry's active-session pointer is defaultChat"
        );
        let mut resources: Vec<&str> = state.chats.iter().map(|c| c.resource.as_str()).collect();
        resources.sort();
        assert_eq!(
            resources,
            vec![
                manox_ahp::channels::chat::uri("chatfold-1").as_str(),
                manox_ahp::channels::chat::uri("chatfold-2").as_str(),
            ],
            "the catalogue lists exactly this thread's journals"
        );
        let dirs = state.working_directories.as_ref().expect("granted dirs");
        assert!(
            dirs.contains(&"file:///tmp/seed".to_string()),
            "the active session's creation cwd is seeded: {dirs:?}"
        );
        assert!(
            dirs.contains(&"file:///work/x".to_string()),
            "a journalled cwdChange grants membership: {dirs:?}"
        );
        let config = state.config.expect("config container");
        assert_eq!(
            config.values.get("model").and_then(|v| v.as_str()),
            Some("newprov/x-model"),
            "model / effort / approval values fold from their rows"
        );
        assert_eq!(
            config
                .values
                .get("workingDirectory")
                .and_then(|v| v.as_str()),
            Some("/work/x")
        );
        let want = SessionStatus::Idle.bits()
            | SessionStatus::IsRead.bits()
            | SessionStatus::InProgress.bits()
            | SessionStatus::IsArchived.bits();
        assert_eq!(
            state.status & want,
            want,
            "live flags + the journalled archive bit are set"
        );
        assert_eq!(
            state.status & SessionStatus::Error.bits(),
            0,
            "no error flag on a clean thread"
        );
    });
    uninstall();
}

#[test]
fn folds_answer_none_for_absent_inputs() {
    let _guards = install();
    manox_agent::runtime::handle().block_on(async {
        let store = manox_agent::thread_store::global();
        store.with_mut(|s| s.insert_summary_for_test("thread-A", None));
        assert!(chat_state("ghost-session").await.is_none());
        assert!(session_state("ghost-thread").await.is_none());
        // A known thread whose journals are all absent still answers:
        // empty catalogue, defaultChat pinned to the registry's pointer.
        let state = session_state("thread-A").await.expect("empty fold");
        assert!(state.chats.is_empty());
        assert_eq!(
            state.default_chat.as_deref(),
            Some(manox_ahp::channels::chat::uri("thread-A").as_str())
        );
    });
    uninstall();
}

// ── dispatch: an accepted action reaches its runtime intent ────────────────
//
// The write path is host-generic (acceptance table → reducer → echo); what is
// manox-specific is that an accepted action must land on the *existing*
// runtime intent rather than a second implementation. These tests pin that
// wiring: each action is dispatched against a real session and the runtime
// side of the effect is observed.
//
// Every action here is built as its **typed** variant rather than parsed from
// JSON: AHP's `StateAction` ends in an untagged `Unknown(Value)` catch-all, so
// a malformed literal would deserialize "successfully" into an action no arm
// matches and the test would assert about the fallback instead of the mapping.

mod dispatch {
    use super::install;
    use super::uninstall;
    use crate::agent_server::AgentServer;
    use ahp_types::actions::{
        ActionOrigin, ChatPendingMessageRemovedAction, ChatToolCallConfirmedAction,
        ChatTurnStartedAction, SessionConfigChangedAction, StateAction,
    };
    use ahp_types::common::JsonObject;
    use ahp_types::state::{Message, MessageKind, MessageOrigin, PendingMessageKind};
    use manox_ahp::backend::{Backend, DispatchOutcome};
    use manox_ahp::channels::{chat, session};
    use serde_json::json;
    use std::sync::Arc;

    /// A server holding one live session, plus the backend under test.
    async fn fixture() -> (
        Arc<AgentServer>,
        Arc<manox_ahp_runtime::ahp::backend::RuntimeBackend>,
    ) {
        let cwd = manox_agent::paths::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
        let server = Arc::new(AgentServer::new_without_store_watcher(cwd.clone()));
        let gateway = Arc::new(crate::ahp_gateway::GatewayRuntime::new(Arc::clone(&server)));
        let backend = manox_ahp_runtime::ahp::backend::RuntimeBackend::new(gateway, cwd);
        let intent = crate::agent_server::SessionIntent {
            session_id: Some("s-dispatch".to_string()),
            cwd: None,
            project: None,
            initial_model: None,
            approval_mode: None,
            reasoning_effort: None,
            seed: None,
            working_directories: Vec::new(),
        };
        let inner = Arc::clone(server.ahp_inner());
        crate::agent_server::AgentServerInner::create_session_request(&inner, "owner", intent)
            .await
            .expect("session opens");
        (server, backend)
    }

    fn origin() -> ActionOrigin {
        ActionOrigin {
            client_id: "client-a".to_string(),
            client_seq: 1,
        }
    }

    fn user_message(text: &str) -> Message {
        Message {
            text: text.to_string(),
            origin: MessageOrigin {
                kind: MessageKind::User,
            },
            attachments: None,
            model: None,
            agent: None,
            meta: None,
        }
    }

    /// `chat/turnStarted` is the turn intent: the runtime is handed the text.
    #[tokio::test(flavor = "multi_thread")]
    async fn turn_started_reaches_the_submit_intent() {
        let _guards = install();
        let (_server, backend) = fixture().await;
        let action = StateAction::ChatTurnStarted(ChatTurnStartedAction {
            turn_id: "t-1".to_string(),
            started_at: "2025-01-02T03:04:05.123Z".to_string(),
            message: user_message("hello"),
            queued_message_id: None,
            meta: None,
        });
        assert_eq!(
            backend.dispatch(&chat::uri("s-dispatch"), &action, &origin()),
            DispatchOutcome::Accepted
        );
        uninstall();
    }

    /// A tool-call confirmation settles the gate under the `authId` the
    /// translator stamped — a guess here would answer a different call.
    #[tokio::test(flavor = "multi_thread")]
    async fn tool_call_confirmation_settles_under_the_stamped_auth_id() {
        let _guards = install();
        let (_server, backend) = fixture().await;
        let mut meta = JsonObject::new();
        meta.insert(
            "x-manox".to_string(),
            json!({"authId": "auth-7", "summary": "run a command"}),
        );
        let action = StateAction::ChatToolCallConfirmed(ChatToolCallConfirmedAction {
            turn_id: "t-1".to_string(),
            tool_call_id: "call-1".to_string(),
            meta: Some(meta),
            approved: true,
            confirmed: Some(ahp_types::state::ToolCallConfirmationReason::UserAction),
            reason: None,
            edited_tool_input: None,
            user_suggestion: None,
            reason_message: None,
            selected_option_id: None,
        });
        assert_eq!(
            backend.dispatch(&chat::uri("s-dispatch"), &action, &origin()),
            DispatchOutcome::Accepted
        );

        // Without the authId the confirmation is unattributable: it must not
        // settle anything, and must not be reported as runtime work.
        let mut unattributed = action.clone();
        if let StateAction::ChatToolCallConfirmed(confirmed) = &mut unattributed {
            confirmed.meta = None;
        }
        assert_eq!(
            backend.dispatch(&chat::uri("s-dispatch"), &unattributed, &origin()),
            DispatchOutcome::Ignored
        );
        uninstall();
    }

    /// `session/configChanged` fans out onto the selection intents.
    #[tokio::test(flavor = "multi_thread")]
    async fn session_config_changed_reaches_the_selection_intents() {
        let _guards = install();
        let (_server, backend) = fixture().await;
        let mut config = JsonObject::new();
        config.insert("approvalMode".to_string(), json!("read-only"));
        config.insert("reasoningEffort".to_string(), json!("high"));
        let action = StateAction::SessionConfigChanged(SessionConfigChangedAction {
            config,
            replace: None,
        });
        assert_eq!(
            backend.dispatch(&session::uri("s-dispatch"), &action, &origin()),
            DispatchOutcome::Accepted
        );
        uninstall();
    }

    /// A submit that lands mid-turn is parked and then drained into a
    /// follow-up turn when the turn settles.
    ///
    /// The drain is the one piece of turn bookkeeping the gateway cannot
    /// delegate: parking without draining strands the user's text forever,
    /// and the receipt it already returned said the submission was accepted.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_mid_turn_submit_is_drained_into_a_follow_up_turn() {
        let _guards = install();
        let (server, _backend) = fixture().await;
        let engine = attach_engine(&server);

        // Park the session as busy the way the kernel does — the watcher's
        // own `TurnStarted` arm is what makes the gateway see a running turn.
        let inner = Arc::clone(server.ahp_inner());
        let thread = inner.session_thread("s-dispatch").expect("live session");
        thread.handle_notice(manox_agent::thread_engine::BackendNotice::Event(Box::new(
            manox_agent::ThreadEvent::TurnStarted,
        )));
        await_until(|| server.turn_active_for_test("s-dispatch")).await;

        // The submit intent is what parks: with the turn active the text
        // cannot run now, so it must wait for the settle edge.
        let parked = inner
            .submit(
                "owner",
                "s-dispatch",
                "queued text".to_string(),
                Vec::new(),
                None,
                None,
            )
            .await
            .expect("a parked submit still answers a receipt");
        assert_eq!(parked["accepted"], serde_json::json!(true));
        assert!(
            engine.prompts.lock().is_empty(),
            "a mid-turn submit must not start a run of its own"
        );

        // Settle: the parked text must become the follow-up run.
        thread.handle_notice(manox_agent::thread_engine::BackendNotice::Settled {
            cancelled: false,
            failed: false,
            steered: Vec::new(),
            stranded: Vec::new(),
        });
        await_until(|| !engine.prompts.lock().is_empty()).await;
        assert_eq!(
            engine.prompts.lock().as_slice(),
            ["queued text"],
            "the drained batch runs as one follow-up turn, with its own text"
        );
        uninstall();
    }

    /// Withdrawing a parked follow-up is real runtime work (the runtime holds
    /// the queue), so it is accepted rather than merely echoed.
    #[tokio::test(flavor = "multi_thread")]
    async fn withdrawing_a_pending_message_reaches_the_queue() {
        let _guards = install();
        let (_server, backend) = fixture().await;
        let action = StateAction::ChatPendingMessageRemoved(ChatPendingMessageRemovedAction {
            kind: PendingMessageKind::Steering,
            id: "p-1".to_string(),
        });
        assert_eq!(
            backend.dispatch(&chat::uri("s-dispatch"), &action, &origin()),
            DispatchOutcome::Accepted
        );
        uninstall();
    }

    /// A client-side action the runtime does not own is echoed as accepted
    /// (`Ignored`) rather than refused: every subscriber must observe one
    /// sequence, and a refusal would contradict state they can already see.
    #[tokio::test(flavor = "multi_thread")]
    async fn client_owned_actions_are_echoed_without_runtime_work() {
        let _guards = install();
        let (_server, backend) = fixture().await;
        let action = StateAction::ChatDraftChanged(ahp_types::actions::ChatDraftChangedAction {
            draft: None,
        });
        assert_eq!(
            backend.dispatch(&chat::uri("s-dispatch"), &action, &origin()),
            DispatchOutcome::Ignored
        );
        uninstall();
    }

    /// A client names the chat URI up front, so a fork must land on *that*
    /// id — not on one the runtime minted behind its back. A client that
    /// subscribes to the chat it asked for must find it there.
    #[tokio::test(flavor = "multi_thread")]
    async fn create_chat_lands_on_the_client_chosen_id() {
        let _guards = install();
        let (_server, backend) = fixture().await;
        // A second chat in the same session needs a source to fork from, and
        // the source needs a completed turn on disk.
        let source = seed_fork_source().await;
        let params = ahp_types::commands::CreateChatParams {
            channel: session::uri("s-dispatch"),
            meta: None,
            chat: chat::uri("client-chosen"),
            initial_message: None,
            source: Some(ahp_types::commands::ChatSource::Fork(
                ahp_types::commands::ForkChatSource {
                    chat: chat::uri(&source),
                    // AHP turn ids are the journal entry id with the
                    // translator's `t-` prefix.
                    turn_id: "t-e-turn".to_string(),
                },
            )),
            working_directories: None,
        };
        backend
            .create_chat("s-dispatch", "client-chosen", &params)
            .expect("the fork lands on the requested id");

        // The same id used twice must be refused, not silently truncated over
        // the journal that already lives there.
        let again = backend.create_chat("s-dispatch", "client-chosen", &params);
        assert!(
            again.is_err(),
            "a colliding chat id must not overwrite the existing journal"
        );
        uninstall();
    }

    /// A `sideChat` keeps its source out of the visible history — a policy the
    /// journal has no row for, so it is refused rather than faked as a fork.
    #[tokio::test(flavor = "multi_thread")]
    async fn create_chat_refuses_a_side_chat_rather_than_faking_one() {
        let _guards = install();
        let (_server, backend) = fixture().await;
        let params = ahp_types::commands::CreateChatParams {
            channel: session::uri("s-dispatch"),
            meta: None,
            chat: chat::uri("side-1"),
            initial_message: None,
            source: Some(ahp_types::commands::ChatSource::SideChat(
                ahp_types::commands::SideChatSource {
                    chat: chat::uri("s-dispatch"),
                    turn_id: "t-1".to_string(),
                    selection: None,
                },
            )),
            working_directories: None,
        };
        assert!(
            backend
                .create_chat("s-dispatch", "side-1", &params)
                .is_err()
        );
        uninstall();
    }

    /// A seeded source session with one completed turn, for the fork tests.
    ///
    /// The fork reads the source journal off disk, so the fixture is a real
    /// `.jsonl` under the store's sessions dir rather than an in-memory state.
    async fn seed_fork_source() -> String {
        super::seed_session(
            "s-source",
            "s-source",
            "/work/src",
            vec![
                ("e-user", super::user("hi")),
                ("e-turn", super::turn_finish()),
            ],
        )
        .await;
        manox_agent::thread_store::global()
            .with_mut(|s| s.insert_summary_for_test("s-source", None));
        "s-source".to_string()
    }

    /// An `x-manox` command the runtime performs reaches its intent; one it
    /// only declares answers `-32080` rather than pretending to work.
    #[tokio::test(flavor = "multi_thread")]
    async fn extension_commands_split_performed_from_declared() {
        let _guards = install();
        let (_server, backend) = fixture().await;
        let params = json!({"channel": session::uri("s-dispatch"), "instructions": "tighten"});
        assert_eq!(
            backend
                .extension(manox_ahp::ext::commands::COMPACT, &params)
                .expect("compact is performed"),
            serde_json::Value::Null
        );

        // Declared, not performed: the honest answer is the extension's own
        // "unsupported" code, not a silent success.
        let err = backend
            .extension(manox_ahp::ext::commands::SHUTDOWN, &params)
            .expect_err("shutdown is not wired");
        assert_eq!(err.code(), manox_ahp::codes::X_MANOX_UNSUPPORTED);

        // A session-scoped command on a non-session channel is a client bug.
        let wrong = json!({"channel": chat::uri("s-dispatch")});
        assert!(
            backend
                .extension(manox_ahp::ext::commands::COMPACT, &wrong)
                .is_err()
        );
        uninstall();
    }

    /// A rename is real work, not a fold-and-forget: the session's title
    /// actually changes, and a blank title is refused instead of echoed (an
    /// echo would fold an empty name while the session kept its old one).
    #[tokio::test(flavor = "multi_thread")]
    async fn title_changed_renames_the_session() {
        let _guards = install();
        let (_server, backend) = fixture().await;
        // A rename is only durable against a session that has a journal, so the
        // happy path needs one (the refusal case is its own test below).
        let session_file =
            manox_agent::thread_store::global_sessions_dir().join("s-dispatch.jsonl");
        std::fs::create_dir_all(session_file.parent().unwrap()).unwrap();
        std::fs::write(
            &session_file,
            "{\"type\":\"session\",\"version\":3,\"id\":\"s-dispatch\",\
             \"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":\"/p\",\
             \"metadata\":{\"host\":\"manox\"}}\n",
        )
        .unwrap();
        manox_agent::thread_store::global().with_mut(|s| {
            s.insert_summary_for_test("s-dispatch", None);
            s.note_session_path("s-dispatch", &session_file);
        });

        let action =
            StateAction::SessionTitleChanged(ahp_types::actions::SessionTitleChangedAction {
                title: "  a better name  ".to_string(),
            });
        assert_eq!(
            backend.dispatch(&session::uri("s-dispatch"), &action, &origin()),
            DispatchOutcome::Accepted
        );
        let stored = manox_agent::thread_store::global().read(|s| {
            s.summary_by_id("s-dispatch")
                .map(|row| row.display_title().to_string())
        });
        assert_eq!(
            stored.as_deref(),
            Some("a better name"),
            "the rename lands, trimmed"
        );

        let blank =
            StateAction::SessionTitleChanged(ahp_types::actions::SessionTitleChangedAction {
                title: "   ".to_string(),
            });
        assert!(
            matches!(
                backend.dispatch(&session::uri("s-dispatch"), &blank, &origin()),
                DispatchOutcome::Rejected(_)
            ),
            "a blank title must not be echoed as accepted"
        );
        uninstall();
    }

    /// A scripted engine that records the intents a dispatch reaches.
    ///
    /// The mappings below matter because their failure is *silent*: a
    /// confirmation that never settles leaves a turn parked forever, and a
    /// cancelled turn that never cancels keeps running. Asserting the dispatch
    /// outcome alone would not catch either — the outcome is `Accepted` even
    /// if the intent is dropped on the floor — so this records the calls.
    #[derive(Default)]
    struct RecordingEngine {
        cancelled: std::sync::atomic::AtomicUsize,
        /// Every prompt the facade handed to a run, in order.
        prompts: parking_lot::Mutex<Vec<String>>,
        questions: parking_lot::Mutex<Vec<(String, String)>>,
        auth: parking_lot::Mutex<Vec<(String, bool)>>,
        cwds: parking_lot::Mutex<Vec<std::path::PathBuf>>,
        steers: parking_lot::Mutex<Vec<String>>,
    }

    impl RecordingEngine {
        fn question_ids(&self) -> Vec<String> {
            self.questions
                .lock()
                .iter()
                .map(|(id, _)| id.clone())
                .collect()
        }
        fn auth_verdicts(&self) -> Vec<(String, bool)> {
            self.auth.lock().clone()
        }
        /// The steer ids the server threaded through (the engine records the
        /// facade's `steer` argument, which carries the client's id).
        fn steer_ids(&self) -> Vec<String> {
            self.steers.lock().clone()
        }
    }

    impl manox_agent::thread_engine::ThreadEngine for RecordingEngine {
        fn is_running(&self) -> bool {
            false
        }
        fn history(&self) -> Vec<manox_agent::db::HistoryEntry> {
            Vec::new()
        }
        // `ThreadHandle::cancel` reaches the engine through `abort`.
        fn abort(&self) {
            self.cancelled
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        fn respond_question(&self, id: &str, outcome: manox_agent::questions::AskOutcome) {
            self.questions
                .lock()
                .push((id.to_string(), format!("{outcome:?}")));
        }
        fn respond_tool_authorization(
            &self,
            id: &str,
            response: manox_agent::permission::ToolAuthorizationResponse,
        ) {
            let allowed = matches!(
                response,
                manox_agent::permission::ToolAuthorizationResponse::Decision(
                    manox_agent::permission::PermissionDecision::AllowOnce
                )
            );
            self.auth.lock().push((id.to_string(), allowed));
        }
        fn set_cwd(&self, path: std::path::PathBuf) {
            self.cwds.lock().push(path);
        }
        fn request_token_usage(
            &self,
        ) -> std::collections::HashMap<String, manox_agent::language_model::TokenUsage> {
            std::collections::HashMap::new()
        }
        fn model(&self) -> Option<manox_harness::types::Model> {
            None
        }
        fn run(&self, prompt: String, _content: Vec<manox_harness::types::ContentBlock>) {
            self.prompts.lock().push(prompt);
        }
        fn steer(
            &self,
            _prompt: String,
            _content: Vec<manox_harness::types::ContentBlock>,
            origin: Option<String>,
        ) -> String {
            let id = origin.unwrap_or_default();
            self.steers.lock().push(id.clone());
            id
        }
        fn cancel_steer(&self, _id: &str) -> bool {
            false
        }
        fn set_model(&self, _model: manox_harness::types::Model) {}
        fn set_thinking_level(&self, _level: Option<String>) {}
        fn open_session(&self, _path: std::path::PathBuf) {}
        fn active_session_path(&self) -> Option<std::path::PathBuf> {
            None
        }
        fn session_list(&self) -> Vec<manox_agent::db::ThreadSummary> {
            Vec::new()
        }
    }

    /// Poll `condition` to a deadline, yielding to the runtime between checks.
    ///
    /// The watcher and the drain run on the agent runtime, so a test that
    /// asserts on their effect has to wait for a task it does not own; a bare
    /// sleep would either be flaky or needlessly slow.
    async fn await_until(condition: impl Fn() -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !condition() {
            assert!(
                std::time::Instant::now() < deadline,
                "condition never became true"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    /// Attach a recording engine to the fixture's session.
    fn attach_engine(server: &Arc<AgentServer>) -> Arc<RecordingEngine> {
        let engine = Arc::new(RecordingEngine::default());
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        server.set_session_engine_for_test("s-dispatch", engine.clone(), rx);
        engine
    }

    /// `chat/inputCompleted` must settle the parked question card. A dropped
    /// settle is silent: the card stays up and the turn never resumes.
    #[tokio::test(flavor = "multi_thread")]
    async fn input_completed_settles_the_question_card() {
        let _guards = install();
        let (server, backend) = fixture().await;
        let engine = attach_engine(&server);

        let action =
            StateAction::ChatInputCompleted(ahp_types::actions::ChatInputCompletedAction {
                request_id: "auth-q1".to_string(),
                response: ahp_types::state::ChatInputResponseKind::Accept,
                answers: None,
            });
        assert_eq!(
            backend.dispatch(&chat::uri("s-dispatch"), &action, &origin()),
            DispatchOutcome::Accepted
        );
        assert_eq!(
            engine.question_ids(),
            vec!["auth-q1".to_string()],
            "the card is settled under the id the request was surfaced with"
        );
        uninstall();
    }

    /// `chat/turnCancelled` must reach the engine's cancel. A dropped cancel is
    /// silent: the UI says stopped while the turn keeps running.
    #[tokio::test(flavor = "multi_thread")]
    async fn turn_cancelled_reaches_the_engine_cancel() {
        let _guards = install();
        let (server, backend) = fixture().await;
        let engine = attach_engine(&server);

        let action = StateAction::ChatTurnCancelled(ahp_types::actions::ChatTurnCancelledAction {
            turn_id: "t-1".to_string(),
            duration: 5,
            meta: None,
        });
        assert_eq!(
            backend.dispatch(&chat::uri("s-dispatch"), &action, &origin()),
            DispatchOutcome::Accepted
        );
        assert_eq!(
            engine.cancelled.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the running turn is cancelled exactly once"
        );
        uninstall();
    }

    /// `session/workingDirectorySet` must reach the engine's cwd. A dropped one
    /// is silent *and* dangerous: every later tool call would be fenced against
    /// the wrong root.
    ///
    /// Two branches exist, and the difference is load-bearing: an *interacted*
    /// session moves its cwd in place (synchronous, asserted here), while a
    /// not-yet-interacted one binds a successor session on a spawned task
    /// (`bind_successor`) and has no synchronous cwd to observe. The session is
    /// therefore marked interacted first, so this pins the in-place leg rather
    /// than racing a spawn.
    #[tokio::test(flavor = "multi_thread")]
    async fn working_directory_set_reaches_the_engine_cwd() {
        let _guards = install();
        let (server, backend) = fixture().await;
        let engine = attach_engine(&server);
        if let Some(thread) = server.ahp_inner().session_thread("s-dispatch") {
            thread.with_mut(|t| t.insert_user_message_with_ui_metadata("first".into(), None));
        }

        let action = StateAction::SessionWorkingDirectorySet(
            ahp_types::actions::SessionWorkingDirectorySetAction {
                directory: "file:///work/granted".to_string(),
            },
        );
        assert_eq!(
            backend.dispatch(&session::uri("s-dispatch"), &action, &origin()),
            DispatchOutcome::Accepted
        );
        assert_eq!(
            engine.cwds.lock().clone(),
            vec![std::path::PathBuf::from("/work/granted")],
            "the new root reaches the engine's fence"
        );

        // A non-file URI cannot name a grant, and must not be accepted as one.
        let bogus = StateAction::SessionWorkingDirectorySet(
            ahp_types::actions::SessionWorkingDirectorySetAction {
                directory: "https://example.com/dir".to_string(),
            },
        );
        assert!(matches!(
            backend.dispatch(&session::uri("s-dispatch"), &bogus, &origin()),
            DispatchOutcome::Rejected(_)
        ));
        assert_eq!(engine.cwds.lock().len(), 1, "the bogus grant never landed");
        uninstall();
    }

    /// A confirmation's verdict must reach the engine as the user's decision —
    /// both directions, since a deny that arrives as an allow is the worst
    /// possible silent failure.
    #[tokio::test(flavor = "multi_thread")]
    async fn tool_call_verdicts_reach_the_engine_both_ways() {
        let _guards = install();
        let (server, backend) = fixture().await;
        let engine = attach_engine(&server);

        for (auth_id, approved) in [("auth-allow", true), ("auth-deny", false)] {
            let mut meta = JsonObject::new();
            meta.insert("x-manox".to_string(), json!({"authId": auth_id}));
            let action = StateAction::ChatToolCallConfirmed(ChatToolCallConfirmedAction {
                turn_id: "t-1".to_string(),
                tool_call_id: format!("call-{auth_id}"),
                meta: Some(meta),
                approved,
                confirmed: None,
                reason: None,
                edited_tool_input: None,
                user_suggestion: None,
                reason_message: None,
                selected_option_id: None,
            });
            assert_eq!(
                backend.dispatch(&chat::uri("s-dispatch"), &action, &origin()),
                DispatchOutcome::Accepted
            );
        }
        assert_eq!(
            engine.auth_verdicts(),
            vec![
                ("auth-allow".to_string(), true),
                ("auth-deny".to_string(), false),
            ],
        );
        uninstall();
    }

    /// `chat/pendingMessageSet` must park the steer under the id the client
    /// chose — that id is what the journal row and the echo retirement share,
    /// so a mismatch would leave a phantom pending message behind.
    #[tokio::test(flavor = "multi_thread")]
    async fn pending_message_set_parks_the_steer_under_its_id() {
        let _guards = install();
        let (server, backend) = fixture().await;
        let engine = attach_engine(&server);
        // A turn must be in flight for the facade to steer rather than send a
        // fresh prompt, and the flag that decides is the *facade's*, not the
        // engine's.
        if let Some(thread) = server.ahp_inner().session_thread("s-dispatch") {
            thread.with_mut(|t| t.set_running_for_test(true));
        }

        let action =
            StateAction::ChatPendingMessageSet(ahp_types::actions::ChatPendingMessageSetAction {
                kind: ahp_types::state::PendingMessageKind::Steering,
                id: "steer-1".to_string(),
                message: user_message("change course"),
            });
        assert_eq!(
            backend.dispatch(&chat::uri("s-dispatch"), &action, &origin()),
            DispatchOutcome::Accepted
        );
        // The injected steer carries the client's id through to the engine.
        let steer_ids = engine.steer_ids();
        assert_eq!(
            steer_ids,
            vec!["steer-1".to_string()],
            "the steer id is threaded, not re-minted"
        );
        uninstall();
    }

    /// `session/isArchivedChanged` must move the row's archived bit. A dropped
    /// archive is silent: the sidebar keeps showing a session the client was
    /// told is gone.
    #[tokio::test(flavor = "multi_thread")]
    async fn is_archived_changed_moves_the_row() {
        let _guards = install();
        let (_server, backend) = fixture().await;
        manox_agent::thread_store::global()
            .with_mut(|s| s.insert_summary_for_test("s-dispatch", None));

        let action = StateAction::SessionIsArchivedChanged(
            ahp_types::actions::SessionIsArchivedChangedAction { is_archived: false },
        );
        assert_eq!(
            backend.dispatch(&session::uri("s-dispatch"), &action, &origin()),
            DispatchOutcome::Accepted
        );
        let archived = manox_agent::thread_store::global()
            .read(|s| s.summary_by_id("s-dispatch").map(|row| row.archived));
        assert_eq!(
            archived,
            Some(false),
            "unarchiving the seeded row is a real, observable state change"
        );
        uninstall();
    }

    /// The rename's **durable** half: the journal row, which is the only route
    /// either protocol face takes.
    ///
    /// The in-memory summary is not the contract. The v2 projection folds
    /// `SessionTreeEntry::Title` and the AHP translator reads the journal,
    /// while the live `TitleChanged` event sits on `translate.rs`'s `Skip`
    /// list — so a rename that only flipped the summary would pass a
    /// summary-only assertion and still be invisible to every client following
    /// the session. This is the cross-protocol hole, pinned.
    ///
    /// A **unique session id** is load-bearing: engine routes are a
    /// process-global map keyed by thread id (`engine::engine_routes`), so
    /// reusing the shared fixture's `s-dispatch` would route this append to
    /// whatever actor an earlier test registered for that id and the row would
    /// never be written.
    #[tokio::test(flavor = "multi_thread")]
    async fn title_changed_appends_the_journal_row() {
        let _guards = install();
        let session_id = format!("s-title-{}", uuid::Uuid::new_v4().simple());
        let session_file =
            manox_agent::thread_store::global_sessions_dir().join(format!("{session_id}.jsonl"));
        std::fs::create_dir_all(session_file.parent().unwrap()).unwrap();
        // The store's cold append (no live engine drives this session) writes to
        // an existing journal, opened through its header row.
        std::fs::write(
            &session_file,
            format!(
                "{{\"type\":\"session\",\"version\":3,\"id\":\"{session_id}\",\
                 \"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":\"/p\",\
                 \"metadata\":{{\"host\":\"manox\"}}}}\n"
            ),
        )
        .unwrap();

        let cwd = manox_agent::paths::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
        let server = Arc::new(AgentServer::new_without_store_watcher(cwd.clone()));
        let gateway = Arc::new(crate::ahp_gateway::GatewayRuntime::new(Arc::clone(&server)));
        let backend = manox_ahp_runtime::ahp::backend::RuntimeBackend::new(gateway, cwd);
        let intent = crate::agent_server::SessionIntent {
            session_id: Some(session_id.clone()),
            cwd: None,
            project: None,
            initial_model: None,
            approval_mode: None,
            reasoning_effort: None,
            seed: None,
            working_directories: Vec::new(),
        };
        let inner = Arc::clone(server.ahp_inner());
        crate::agent_server::AgentServerInner::create_session_request(&inner, "owner", intent)
            .await
            .expect("session opens");
        manox_agent::thread_store::global().with_mut(|s| {
            s.insert_summary_for_test(&session_id, None);
            s.note_session_path(&session_id, &session_file);
        });

        let action =
            StateAction::SessionTitleChanged(ahp_types::actions::SessionTitleChangedAction {
                title: "durable name".to_string(),
            });
        assert_eq!(
            backend.dispatch(&session::uri(&session_id), &action, &origin()),
            DispatchOutcome::Accepted
        );

        let mut titles: Vec<String> = Vec::new();
        for _ in 0..500 {
            let storage = manox_harness::session::jsonl::JsonlSessionStorage::open(&session_file)
                .await
                .expect("session file opens");
            titles = storage
                .journal_range(0, u64::MAX)
                .await
                .expect("journal reads")
                .into_iter()
                .filter_map(|record| match record.entry {
                    manox_harness::session::SessionTreeEntry::Title { title, .. } => Some(title),
                    _ => None,
                })
                .collect();
            if !titles.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            titles,
            vec!["durable name".to_string()],
            "the rename must land a journal title row, not only the summary"
        );
        uninstall();
    }

    /// The two refusal reasons are distinct, and the blank one is the one a
    /// client can act on.
    ///
    /// A rename fails for two unrelated causes — an empty title (the client's
    /// fault) and a title that cannot be made durable (the environment's) — and
    /// a two-valued return reported the second as the first, so a client asking
    /// to rename to a perfectly good string was told its title was blank. This
    /// pins the blank reason here; the undurable one is pinned at the store,
    /// where it can be produced deterministically (`rename_thread`'s own test),
    /// because whether a live engine route exists is process-global state this
    /// suite cannot control.
    #[tokio::test(flavor = "multi_thread")]
    async fn title_changed_refusal_names_the_blank_title() {
        let _guards = install();
        let (_server, backend) = fixture().await;
        manox_agent::thread_store::global()
            .with_mut(|s| s.insert_summary_for_test("s-dispatch", None));

        let action =
            StateAction::SessionTitleChanged(ahp_types::actions::SessionTitleChangedAction {
                title: "   ".to_string(),
            });
        match backend.dispatch(&session::uri("s-dispatch"), &action, &origin()) {
            DispatchOutcome::Rejected(reason) => assert!(
                reason.contains("blank"),
                "a blank title names itself as the cause: {reason}"
            ),
            other => panic!("a blank title must be refused, got {other:?}"),
        }

        // An unknown session is its own reason again, not "blank".
        let unknown =
            StateAction::SessionTitleChanged(ahp_types::actions::SessionTitleChangedAction {
                title: "fine".to_string(),
            });
        match backend.dispatch(&session::uri("s-absent-entirely"), &unknown, &origin()) {
            DispatchOutcome::Rejected(reason) => assert!(
                reason.contains("unknown session"),
                "an unknown session names itself: {reason}"
            ),
            other => panic!("an unknown session must be refused, got {other:?}"),
        }
        uninstall();
    }

    /// A brand-new session must be seedable before any list refresh has scanned
    /// its file.
    ///
    /// `createSession` seeds the session it just created, and its store row only
    /// appears once a refresh scans the new file. Requiring that row made the
    /// host answer `session not found` for the session it had itself created —
    /// and it is the very next step of the same request, so the command failed
    /// on its own subject. The empty state is what such a session is.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_fresh_session_seeds_before_its_store_row_exists() {
        let _guards = install();
        let session_id = format!("s-fresh-{}", uuid::Uuid::new_v4().simple());
        let cwd = manox_agent::paths::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
        let server = Arc::new(AgentServer::new_without_store_watcher(cwd.clone()));
        let gateway = Arc::new(crate::ahp_gateway::GatewayRuntime::new(Arc::clone(&server)));
        let backend = manox_ahp_runtime::ahp::backend::RuntimeBackend::new(gateway, cwd);
        let intent = crate::agent_server::SessionIntent {
            session_id: Some(session_id.clone()),
            cwd: None,
            project: None,
            initial_model: None,
            approval_mode: None,
            reasoning_effort: None,
            seed: None,
            working_directories: Vec::new(),
        };
        let inner = Arc::clone(server.ahp_inner());
        crate::agent_server::AgentServerInner::create_session_request(&inner, "owner", intent)
            .await
            .expect("session opens");

        // No store row was inserted: this is the state `createSession` seeds in.
        let seeded = backend.session_state(&session_id);
        assert!(
            seeded.is_some(),
            "a fresh session seeds from an empty state, not `None`"
        );
        assert_eq!(
            seeded.unwrap().default_chat.as_deref(),
            Some(chat::uri(&session_id).as_str()),
            "the fresh session's active pointer is itself"
        );
        uninstall();
    }

    /// A session the client created is addressable by the id it chose, and a
    /// rename on it reports the truth about durability.
    ///
    /// Two defects met here. `createSession` seeds what it just created, and a
    /// fresh session's store row only appears after a list refresh scans its
    /// new file — so the seed must tolerate the missing row (it answered
    /// `session not found` for the session it had itself created). The rename
    /// then hit the same gap from the other side: the store knows the session by
    /// path (the create path notes it) but has no summary row, which the store
    /// read as "unknown session".
    ///
    /// The remaining refusal is correct and is the point: a fresh session's
    /// `.jsonl` materializes lazily at its first turn, so until then the rename
    /// has no journal to be durable in — and saying so is the honest answer.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_created_session_is_addressable_and_reports_durability() {
        let _guards = install();
        let id = format!("s-created-{}", uuid::Uuid::new_v4().simple());
        let cwd = manox_agent::paths::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
        let server = Arc::new(AgentServer::new_without_store_watcher(cwd.clone()));
        let gateway = Arc::new(crate::ahp_gateway::GatewayRuntime::new(Arc::clone(&server)));
        let backend = manox_ahp_runtime::ahp::backend::RuntimeBackend::new(gateway, cwd);
        let params = ahp_types::commands::CreateSessionParams {
            channel: session::uri(&id),
            meta: None,
            provider: None,
            working_directories: Some(vec!["file:///tmp".to_string()]),
            config: None,
            active_client: None,
            progress_token: None,
        };
        backend.create_session(&id, &params).expect("creates");
        assert!(
            server.ahp_inner().session_thread(&id).is_some(),
            "the runtime addresses the session by the client's chosen id"
        );
        assert!(
            backend.chat_state(&id).is_some(),
            "the AHP chat channel resolves for the created session"
        );

        let action =
            StateAction::SessionTitleChanged(ahp_types::actions::SessionTitleChangedAction {
                title: "created rename".to_string(),
            });
        match backend.dispatch(&session::uri(&id), &action, &origin()) {
            DispatchOutcome::Rejected(reason) => assert!(
                reason.contains("not writable"),
                "the reason is durability, not identity: {reason}"
            ),
            DispatchOutcome::Accepted => {
                // Acceptable only when the journal really exists — an accepted
                // rename must have a durable row behind it, and a live engine
                // route alone is not that (a queued append to an engine that
                // never opened a file is dropped without a trace).
                assert!(
                    manox_agent::thread_store::global_sessions_dir()
                        .join(format!("{id}.jsonl"))
                        .exists(),
                    "an accepted rename must have a journal behind it"
                );
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
        uninstall();
    }

    /// Subscribing to a declared extension channel must deliver its state, not
    /// silence.
    ///
    /// `_meta["x-manox"]` advertises six channels; none is state-bearing, so
    /// `subscribe` has no snapshot and the baseline is the only thing a client
    /// can receive. Returning nothing made the declaration a promise the host
    /// did not keep — the failure mode is a subscriber that waits forever with
    /// no error to report.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_declared_extension_channel_answers_with_a_baseline() {
        let _guards = install();
        let (_server, backend) = fixture().await;
        manox_agent::thread_store::global()
            .with_mut(|s| s.insert_summary_for_test("s-dispatch", None));

        // Every declared channel answers. The per-session ones carry the
        // session id; the catalogue ones describe the host and take none.
        for prefix in manox_ahp::ext::channels::ALL {
            let channel = if prefix.ends_with(":/") {
                format!("{prefix}s-dispatch")
            } else {
                (*prefix).to_string()
            };
            let baseline = backend
                .extension_baseline(&channel)
                .unwrap_or_else(|| panic!("{channel} is declared and must answer"));
            assert_eq!(baseline.0, manox_ahp::ext::BASELINE_NOTIFICATION);
            assert_eq!(
                baseline.1["channel"], channel,
                "the baseline names the channel it describes"
            );
        }

        // A session-scoped channel names its session in the URI; the baseline
        // echoes the channel so a client can correlate it. (Existence is the
        // subscribe path's job — `ensure_session` runs before this — so a
        // baseline is not the place to re-litigate it.)

        // A channel that is not ours is not answered here (the standard
        // channels take the snapshot path).
        assert!(
            backend
                .extension_baseline("ahp-session:/s-dispatch")
                .is_none()
        );
        assert!(backend.extension_baseline("ahp-chat:/s-dispatch").is_none());
        uninstall();
    }

    /// The baseline is the journal's own fold, so it carries the state a live
    /// subscriber would have reached — not an empty placeholder.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_extension_baseline_carries_the_folded_state() {
        use manox_harness::session::SessionTreeEntry as E;
        let _guards = install();
        let session_id = "s-ext-baseline";
        super::seed_session(
            session_id,
            session_id,
            "/work/src",
            vec![
                (
                    "e-1",
                    E::PlanModeChange {
                        id: String::new(),
                        parent_id: None,
                        timestamp: super::stamp(),
                        enabled: true,
                    },
                ),
                (
                    "e-2",
                    E::Title {
                        id: String::new(),
                        parent_id: None,
                        timestamp: super::stamp(),
                        title: "extension baseline".to_string(),
                    },
                ),
            ],
        )
        .await;
        manox_agent::thread_store::global()
            .with_mut(|s| s.insert_summary_for_test(session_id, None));

        let cwd = manox_agent::paths::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
        let server = Arc::new(AgentServer::new_without_store_watcher(cwd.clone()));
        let gateway = Arc::new(crate::ahp_gateway::GatewayRuntime::new(Arc::clone(&server)));
        let backend = manox_ahp_runtime::ahp::backend::RuntimeBackend::new(gateway, cwd);

        let channel = format!("{}{session_id}", manox_ahp::ext::channels::PLAN);
        let (method, payload) = backend
            .extension_baseline(&channel)
            .expect("the plan channel answers");
        assert_eq!(method, manox_ahp::ext::BASELINE_NOTIFICATION);
        assert_eq!(
            payload["state"]["planMode"], true,
            "the baseline is the journal's fold, not an empty placeholder: {payload}"
        );
        uninstall();
    }

    /// The terminal channel serves real state, not a declaration.
    ///
    /// `ahp-terminal:/<id>` was parseable but unserved: `terminal_state` returned
    /// `None`, so a client could subscribe and only ever get `not found`. A
    /// channel that parses but never answers is worse than one that is not
    /// declared at all — the failure is a silent wait, not an error.
    #[cfg(feature = "terminal")]
    #[tokio::test(flavor = "multi_thread")]
    async fn the_terminal_channel_serves_live_state() {
        let _guards = install();
        let (server, backend) = fixture().await;

        let attached = server
            .ahp_inner()
            .attach_terminal("s-dispatch", 80, 24, None)
            .expect("a terminal spawns");
        let terminal_id = attached
            .get("terminal_id")
            .and_then(|v| v.as_str())
            .expect("the attach response names the terminal")
            .to_string();

        let state = backend
            .terminal_state(&terminal_id)
            .expect("the terminal channel answers for a live terminal");
        assert_eq!(state.cols, Some(80));
        assert_eq!(state.rows, Some(24));
        assert!(
            matches!(
                state.lifecycle,
                ahp_types::state::TerminalLifecycleState::Running(_)
            ),
            "a freshly spawned terminal is running"
        );
        assert_eq!(state.is_pty, Some(true));
        assert!(
            !matches!(state.claim, ahp_types::state::TerminalClaim::Client(_)),
            "the runtime does not arbitrate client input ownership, so it must not \
             announce a client claim it would not enforce"
        );

        // The write path: keystrokes and resizes reach the PTY. Without these
        // the client can create and watch a terminal but never type into it,
        // which is not a usable terminal — and v2 serves both.
        assert_eq!(
            backend.dispatch(
                &manox_ahp::channels::terminal::uri(&terminal_id),
                &StateAction::TerminalInput(ahp_types::actions::TerminalInputAction {
                    data: "echo hi\n".to_string(),
                }),
                &origin(),
            ),
            DispatchOutcome::Accepted
        );
        assert_eq!(
            backend.dispatch(
                &manox_ahp::channels::terminal::uri(&terminal_id),
                &StateAction::TerminalResized(ahp_types::actions::TerminalResizedAction {
                    cols: 100,
                    rows: 30,
                }),
                &origin(),
            ),
            DispatchOutcome::Accepted
        );

        // An out-of-range size is refused, not clamped: clamping would resize
        // the PTY to something the client did not ask for.
        assert!(matches!(
            backend.dispatch(
                &manox_ahp::channels::terminal::uri(&terminal_id),
                &StateAction::TerminalResized(ahp_types::actions::TerminalResizedAction {
                    cols: 100_000,
                    rows: 30,
                }),
                &origin(),
            ),
            DispatchOutcome::Rejected(_)
        ));

        // Input to a terminal that does not exist is refused, never silently
        // dropped (the client would type into nothing and never learn).
        assert!(matches!(
            backend.dispatch(
                &manox_ahp::channels::terminal::uri("no-such-terminal"),
                &StateAction::TerminalInput(ahp_types::actions::TerminalInputAction {
                    data: "x".to_string(),
                }),
                &origin(),
            ),
            DispatchOutcome::Rejected(_)
        ));

        // Disposal releases the PTY. The watcher task holds an `Arc` of the
        // entry, so this only holds if disposal breaks that cycle — otherwise
        // the map entry is gone, `disposeTerminal` reports success, and the
        // child shell keeps running.
        assert!(backend.dispose_terminal(&terminal_id).is_ok());
        assert!(
            backend.terminal_state(&terminal_id).is_none(),
            "a disposed terminal is gone from the channel"
        );

        // An unknown terminal is absent, never fabricated.
        assert!(backend.terminal_state("no-such-terminal").is_none());
        uninstall();
    }

    /// The capability router's AHP leg declines cleanly when the transport has
    /// no capable client, so the v2 path still runs.
    ///
    /// The positive half — an AHP client that declared a capability is the one
    /// asked — is pinned in `manox-ahp`'s `only_declared_client_requests_select_a_connection`,
    /// where a `Conn` is reachable. What this covers is the other direction: a
    /// subscription without a declaration must not make the AHP host look like
    /// the owner, because the router would then ask a client that never claimed
    /// the capability and wait out the deadline for an answer that cannot come.
    #[tokio::test(flavor = "multi_thread")]
    async fn capability_routing_declines_when_no_ahp_client_declared_it() {
        use ahp::ClientConfig;
        let _guards = install();
        let (server, _backend) = fixture().await;
        manox_agent::thread_store::global()
            .with_mut(|s| s.insert_summary_for_test("s-dispatch", None));

        let cwd = manox_agent::paths::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
        let gateway = Arc::new(crate::ahp_gateway::GatewayRuntime::new(Arc::clone(&server)));
        let runtime = manox_ahp_runtime::ahp::runtime::AhpRuntime::new(gateway, cwd);

        // A client that subscribes without declaring anything.
        let client = ahp::Client::connect(runtime.inproc(), ClientConfig::default())
            .await
            .expect("connects");
        client
            .initialize(
                "silent".to_string(),
                vec![ahp_types::version::PROTOCOL_VERSION.to_string()],
                vec![session::uri("s-dispatch")],
            )
            .await
            .expect("initializes");

        for method in [
            manox_ahp::ext::requests::CLIPBOARD_READ,
            manox_ahp::ext::requests::BROWSER_OP,
            manox_ahp::ext::requests::OPEN_EXTERNAL,
        ] {
            assert!(
                !runtime.has_capable_client("s-dispatch", method),
                "an undeclared capability must not select this transport: {method}"
            );
        }
        uninstall();
    }

    /// Pin and manual ordering are user-visible v2 features with no AHP slot, so
    /// they ride the declared extension surface. Both must reach their durable
    /// layer — a pin the sidebar shows and a cold restore forgets would be the
    /// worst of both.
    #[tokio::test(flavor = "multi_thread")]
    async fn pin_and_order_reach_their_durable_layers() {
        use manox_agent::thread_store;
        let _guards = install();
        let (_server, backend) = fixture().await;
        thread_store::global().with_mut(|s| {
            s.insert_summary_for_test("s-dispatch", None);
            s.insert_summary_for_test("s-other", None);
        });

        // Pin: the store row is the read-back surface.
        let pin = StateAction::Unknown(json!({
            "type": manox_ahp::ext::actions::PINNED_CHANGED,
            "pinned": true,
        }));
        assert_eq!(
            backend.dispatch(&session::uri("s-dispatch"), &pin, &origin()),
            DispatchOutcome::Accepted
        );
        let pinned =
            thread_store::global().read(|s| s.summary_by_id("s-dispatch").map(|row| row.pinned));
        assert_eq!(pinned, Some(true), "the pin lands on the durable row");

        // Order: `before` names an anchor; null means the head.
        let order = StateAction::Unknown(json!({
            "type": manox_ahp::ext::actions::ORDER_CHANGED,
            "before": "s-other",
        }));
        assert_eq!(
            backend.dispatch(&session::uri("s-dispatch"), &order, &origin()),
            DispatchOutcome::Accepted
        );

        // Both refuse an unknown session rather than reporting a success the
        // sidebar would not show.
        assert!(matches!(
            backend.dispatch(&session::uri("s-absent"), &pin, &origin()),
            DispatchOutcome::Rejected(_)
        ));
        assert!(matches!(
            backend.dispatch(&session::uri("s-absent"), &order, &origin()),
            DispatchOutcome::Rejected(_)
        ));

        // A malformed payload is refused by name, not folded.
        assert!(matches!(
            backend.dispatch(
                &session::uri("s-dispatch"),
                &StateAction::Unknown(json!({
                    "type": manox_ahp::ext::actions::PINNED_CHANGED,
                    "pinned": "yes",
                })),
                &origin(),
            ),
            DispatchOutcome::Rejected(_)
        ));
        assert!(matches!(
            backend.dispatch(
                &session::uri("s-dispatch"),
                &StateAction::Unknown(json!({
                    "type": manox_ahp::ext::actions::ORDER_CHANGED,
                    "before": 42,
                })),
                &origin(),
            ),
            DispatchOutcome::Rejected(_)
        ));
        uninstall();
    }

    /// An action with no intent at all is refused loudly — the trait contract
    /// is that a refused write never looks accepted.
    #[tokio::test(flavor = "multi_thread")]
    async fn unwired_actions_are_refused_with_a_reason() {
        let _guards = install();
        let (_server, backend) = fixture().await;
        // `chat/truncated` is on the acceptance table (a client may dispatch it)
        // but has no runtime intent yet, so it must be refused by name rather
        // than folded and echoed as if something had happened.
        let action = StateAction::ChatTruncated(ahp_types::actions::ChatTruncatedAction {
            turn_id: Some("t-1".to_string()),
        });
        match backend.dispatch(&chat::uri("s-dispatch"), &action, &origin()) {
            DispatchOutcome::Rejected(reason) => {
                assert!(
                    reason.contains("chat/truncated"),
                    "names the action: {reason}"
                );
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        uninstall();
    }

    // ── client-contributed tools (`session/activeClientSet`) ───────────────
    //
    // The AHP face of v2's `RegisterSessionTools`. The registration lands in
    // the runtime's embedder-tool store (so the model can call the tool) while
    // `activeClients` carries it on the protocol surface (so every subscriber
    // reads it). Both halves are asserted here, because either alone is a
    // half-built capability.

    /// One contributed tool definition, built typed rather than from JSON.
    fn tool_definition(name: &str) -> ahp_types::state::ToolDefinition {
        ahp_types::state::ToolDefinition {
            name: name.to_string(),
            title: None,
            description: Some(format!("the {name} tool")),
            input_schema: Some(json!({
                "type": "object",
                "properties": { "q": { "type": "string" } },
            })),
            output_schema: None,
            annotations: None,
            meta: None,
        }
    }

    /// An active-client entry as a registering client dispatches it.
    fn active_client(
        client_id: &str,
        tools: Vec<ahp_types::state::ToolDefinition>,
    ) -> ahp_types::state::SessionActiveClient {
        ahp_types::state::SessionActiveClient {
            client_id: client_id.to_string(),
            display_name: Some("Test Client".to_string()),
            tools,
            customizations: None,
        }
    }

    fn active_client_set(client: ahp_types::state::SessionActiveClient) -> StateAction {
        StateAction::SessionActiveClientSet(ahp_types::actions::SessionActiveClientSetAction {
            active_client: client,
        })
    }

    /// A registration reaches the runtime's tool store, so the model can call
    /// the tool: the AHP action and the v2 `RegisterSessionTools` call fill the
    /// same registration the engine consults at assembly.
    #[tokio::test(flavor = "multi_thread")]
    async fn active_client_set_registers_tools_the_runtime_will_mount() {
        let _guards = install();
        let (server, backend) = fixture().await;
        manox_agent::embedder_tools::drop_provider_for_test();

        let action = active_client_set(active_client(
            "client-a",
            vec![tool_definition("get_selection")],
        ));
        assert_eq!(
            backend.dispatch(&session::uri("s-dispatch"), &action, &origin()),
            DispatchOutcome::Accepted
        );

        // The runtime half: the engine-facing provider now hands out the tool,
        // under the `client_` model-facing name v2 uses for the same fact.
        let provider = crate::agent_server::AgentServerEmbedderTools::new(&server);
        use manox_agent::embedder_tools::EmbedderToolProvider as _;
        let mounted = provider.tools_for("s-dispatch");
        assert_eq!(mounted.len(), 1, "the registration reached the provider");
        assert_eq!(mounted[0].name(), "client_get_selection");
        assert_eq!(mounted[0].description(), "the get_selection tool");
        assert_eq!(
            mounted[0].parameters_schema(),
            json!({"type": "object", "properties": { "q": { "type": "string" } }}),
            "the registrant's schema is carried verbatim"
        );
        uninstall();
    }

    /// Re-registering replaces the set (full replacement per client), which is
    /// both the AHP upsert contract and the v2 call's own semantics.
    #[tokio::test(flavor = "multi_thread")]
    async fn active_client_set_replaces_a_previous_registration() {
        let _guards = install();
        let (server, backend) = fixture().await;
        manox_agent::embedder_tools::drop_provider_for_test();

        let first = active_client_set(active_client("client-a", vec![tool_definition("old")]));
        assert_eq!(
            backend.dispatch(&session::uri("s-dispatch"), &first, &origin()),
            DispatchOutcome::Accepted
        );
        let second = active_client_set(active_client("client-a", vec![tool_definition("new")]));
        assert_eq!(
            backend.dispatch(&session::uri("s-dispatch"), &second, &origin()),
            DispatchOutcome::Accepted
        );

        let provider = crate::agent_server::AgentServerEmbedderTools::new(&server);
        use manox_agent::embedder_tools::EmbedderToolProvider as _;
        let mounted = provider.tools_for("s-dispatch");
        assert_eq!(
            mounted.iter().map(|t| t.name()).collect::<Vec<_>>(),
            vec!["client_new"],
            "the previous set is replaced, not merged"
        );
        uninstall();
    }

    /// Two clients keep their own sets: `activeClients` is keyed by `clientId`,
    /// so one client's re-registration must not evict another's tools.
    #[tokio::test(flavor = "multi_thread")]
    async fn active_client_set_is_scoped_to_the_registering_client() {
        let _guards = install();
        let (server, backend) = fixture().await;
        manox_agent::embedder_tools::drop_provider_for_test();

        for (client, tool) in [("client-a", "alpha"), ("client-b", "beta")] {
            let action = active_client_set(active_client(client, vec![tool_definition(tool)]));
            assert_eq!(
                backend.dispatch(&session::uri("s-dispatch"), &action, &origin()),
                DispatchOutcome::Accepted
            );
        }

        let provider = crate::agent_server::AgentServerEmbedderTools::new(&server);
        use manox_agent::embedder_tools::EmbedderToolProvider as _;
        let mut names: Vec<String> = provider
            .tools_for("s-dispatch")
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        names.sort();
        assert_eq!(names, vec!["client_alpha", "client_beta"]);
        uninstall();
    }

    /// A registration against a session this runtime does not drive is refused
    /// loudly. Accepting it would fold a tool set into every subscriber's view
    /// of a session whose engine will never mount it, so the model would be
    /// offered a tool that cannot run.
    #[tokio::test(flavor = "multi_thread")]
    async fn active_client_set_on_an_unknown_session_is_refused() {
        let _guards = install();
        let (_server, backend) = fixture().await;
        let action = active_client_set(active_client("client-a", vec![tool_definition("t")]));
        match backend.dispatch(&session::uri("no-such-session"), &action, &origin()) {
            DispatchOutcome::Rejected(reason) => {
                assert!(
                    reason.contains("no-such-session"),
                    "names the session: {reason}"
                );
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        uninstall();
    }

    /// A malformed registration is refused rather than stored: a nameless tool
    /// cannot be called, and a blank name would sanitize into a collision with
    /// another client's tool.
    #[tokio::test(flavor = "multi_thread")]
    async fn active_client_set_with_a_nameless_tool_is_refused() {
        let _guards = install();
        let (_server, backend) = fixture().await;
        let action = active_client_set(active_client("client-a", vec![tool_definition("  ")]));
        assert!(
            matches!(
                backend.dispatch(&session::uri("s-dispatch"), &action, &origin()),
                DispatchOutcome::Rejected(_)
            ),
            "a tool definition without a name is not registrable"
        );
        uninstall();
    }

    /// A registration with no client id is refused: it would land in the store
    /// under an empty key, where nothing can route a subsequent invocation back
    /// to the client that contributed the tool.
    #[tokio::test(flavor = "multi_thread")]
    async fn active_client_set_without_a_client_id_is_refused() {
        let _guards = install();
        let (_server, backend) = fixture().await;
        let action = active_client_set(active_client("", vec![tool_definition("t")]));
        assert!(matches!(
            backend.dispatch(&session::uri("s-dispatch"), &action, &origin()),
            DispatchOutcome::Rejected(_)
        ));
        uninstall();
    }

    /// A tool with no declared schema is still registrable — AHP makes the
    /// schema optional for client tools — and defaults to the permissive object
    /// schema rather than being refused.
    #[tokio::test(flavor = "multi_thread")]
    async fn active_client_set_accepts_a_tool_without_a_schema() {
        let _guards = install();
        let (server, backend) = fixture().await;
        manox_agent::embedder_tools::drop_provider_for_test();
        let mut tool = tool_definition("schemaless");
        tool.input_schema = None;
        let action = active_client_set(active_client("client-a", vec![tool]));

        assert_eq!(
            backend.dispatch(&session::uri("s-dispatch"), &action, &origin()),
            DispatchOutcome::Accepted
        );
        let provider = crate::agent_server::AgentServerEmbedderTools::new(&server);
        use manox_agent::embedder_tools::EmbedderToolProvider as _;
        assert_eq!(
            provider.tools_for("s-dispatch")[0].parameters_schema(),
            json!({ "type": "object" })
        );
        uninstall();
    }

    /// The `readOnlyHint` annotation survives registration: the approval gate
    /// reads it, so losing it would silently gate a read-only contributor.
    #[tokio::test(flavor = "multi_thread")]
    async fn active_client_set_carries_the_read_only_hint() {
        let _guards = install();
        let (server, backend) = fixture().await;
        manox_agent::embedder_tools::drop_provider_for_test();
        let mut tool = tool_definition("reader");
        tool.annotations = Some(ahp_types::state::ToolAnnotations {
            title: None,
            read_only_hint: Some(true),
            destructive_hint: None,
            idempotent_hint: None,
            open_world_hint: None,
        });
        let action = active_client_set(active_client("client-a", vec![tool]));
        assert_eq!(
            backend.dispatch(&session::uri("s-dispatch"), &action, &origin()),
            DispatchOutcome::Accepted
        );

        let provider = crate::agent_server::AgentServerEmbedderTools::new(&server);
        use manox_agent::embedder_tools::EmbedderToolProvider as _;
        let mounted = provider.tools_for("s-dispatch");
        assert!(mounted[0].is_read_only());
        assert!(!mounted[0].requires_approval(&json!({})));
        uninstall();
    }
}

/// The two-transport parity test (see its own docs).
mod transport_parity {
    use super::*;
    use crate::agent_server::AgentServer;
    use ahp::ClientConfig;
    use ahp_types::version::PROTOCOL_VERSION;
    use manox_ahp_runtime::ahp::runtime::AhpRuntime;
    use std::path::PathBuf;

    /// Both legs serve one host: a root action published once arrives at the
    /// in-process subscriber and the WebSocket subscriber with the same
    /// `serverSeq` — the property that lets a local window and a remote client
    /// share a session without a second gateway in the process.
    #[tokio::test(flavor = "multi_thread")]
    async fn in_process_and_websocket_legs_share_one_host() {
        // The crate's established test scaffolding (same shape as the fold
        // suite): a hermetic HOME, a standalone threads db and a scratch
        // registry file, guarded so the overrides cannot leak across suites.
        let outer = crate::test_support::lock_globals();
        let store_lock = manox_agent::thread_store::store_test_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        crate::test_support::hermetic_home();
        crate::test_support::init_globals();
        let scratch = std::env::temp_dir().join(format!(
            "manox-ahp-parity-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&scratch).expect("scratch dir");
        let db = Arc::new(
            manox_agent::db::ThreadsDatabase::open(&scratch.join("threads.db"))
                .expect("threads db"),
        );
        manox_agent::thread_store::init_for_test(db);
        manox_agent::thread_registry::set_registry_path_for_test(Some(
            scratch.join("threads.registry.json"),
        ));
        let _guards = (outer, store_lock);

        let cwd = manox_agent::paths::home_dir().unwrap_or_else(|| PathBuf::from("."));
        let server = Arc::new(AgentServer::new_without_store_watcher(cwd.clone()));
        let gateway = Arc::new(crate::ahp_gateway::GatewayRuntime::new(Arc::clone(&server)));
        let runtime = AhpRuntime::new(gateway, cwd);

        // Leg one: over channel (in-process).
        let inproc = ahp::Client::connect(runtime.inproc(), ClientConfig::default())
            .await
            .expect("in-process client connects");
        let inproc_init = inproc
            .initialize(
                "desktop".to_string(),
                vec![PROTOCOL_VERSION.to_string()],
                vec![ahp_types::common::ROOT_RESOURCE_URI.to_string()],
            )
            .await
            .expect("in-process initialize");
        let mut inproc_root = inproc
            .attach_subscription(ahp_types::common::ROOT_RESOURCE_URI)
            .await;

        // Leg two: over websocket, through the gateway-shaped router.
        let app = runtime.router("/ahp");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("binds loopback");
        let addr = listener.local_addr().expect("bound address");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let client_transport = ahp_ws::WebSocketTransport::connect(&format!("ws://{addr}/ahp"))
            .await
            .expect("websocket client connects");
        let remote = ahp::Client::connect(client_transport, ClientConfig::default())
            .await
            .expect("websocket client ready");
        let remote_init = remote
            .initialize(
                "remote".to_string(),
                vec![PROTOCOL_VERSION.to_string()],
                vec![ahp_types::common::ROOT_RESOURCE_URI.to_string()],
            )
            .await
            .expect("websocket initialize");
        let mut remote_root = remote
            .attach_subscription(ahp_types::common::ROOT_RESOURCE_URI)
            .await;

        // Same host, same snapshot.
        assert_eq!(
            serde_json::to_value(&inproc_init.snapshots[0].state).unwrap(),
            serde_json::to_value(&remote_init.snapshots[0].state).unwrap(),
        );
        let meta = remote_init
            .meta
            .expect("the extension surface is advertised");
        assert_eq!(meta["x-manox"]["version"], 1);

        // One publish, two subscribers, one sequence number.
        let published = runtime.host().publish(
            ahp_types::common::ROOT_RESOURCE_URI,
            ahp_types::actions::StateAction::RootAgentsChanged(
                ahp_types::actions::RootAgentsChangedAction {
                    agents: runtime.host().backend().root_state().agents,
                },
            ),
            None,
        );
        let inproc_envelope = next_action(&mut inproc_root).await;
        let remote_envelope = next_action(&mut remote_root).await;
        assert_eq!(inproc_envelope.server_seq, published.server_seq);
        assert_eq!(remote_envelope.server_seq, published.server_seq);

        manox_agent::thread_registry::set_registry_path_for_test(None);
        manox_agent::thread_store::drop_for_test();
    }

    async fn next_action(sub: &mut ahp::SessionSubscription) -> ahp_types::actions::ActionEnvelope {
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), sub.recv())
            .await
            .expect("action arrives")
            .expect("subscription open");
        match event {
            ahp::SubscriptionEvent::Action(envelope) => envelope,
            other => panic!("expected an action envelope, got {other:?}"),
        }
    }
}
