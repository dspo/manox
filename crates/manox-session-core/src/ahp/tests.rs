//! Fold tests: a seeded journal on disk must reach the AHP chat/session
//! states the same host-and-client fold produces (§C, L10).

use super::*;
use ahp_types::state::{ResponsePart, ToolCallState, ToolResultContent, TurnState};
use chrono::{DateTime, Utc};
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
    use manox_ahp::channels::{chat, session, terminal};
    use serde_json::json;
    use std::sync::Arc;

    /// A server holding one live session, plus the backend under test.
    async fn fixture() -> (Arc<AgentServer>, Arc<super::super::backend::RuntimeBackend>) {
        let cwd = manox_agent::paths::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
        let server = Arc::new(AgentServer::new_without_store_watcher(cwd.clone()));
        let backend = super::super::backend::RuntimeBackend::new(Arc::clone(&server), cwd);
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
        config.insert("approvalMode".to_string(), json!("plan"));
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
        fn run(&self, _prompt: String, _content: Vec<manox_harness::types::ContentBlock>) {}
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
        let backend = super::super::backend::RuntimeBackend::new(Arc::clone(&server), cwd);
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
        let backend = super::super::backend::RuntimeBackend::new(Arc::clone(&server), cwd);
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
        let backend = super::super::backend::RuntimeBackend::new(Arc::clone(&server), cwd);
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

    /// An action with no intent at all is refused loudly — the trait contract
    /// is that a refused write never looks accepted.
    #[tokio::test(flavor = "multi_thread")]
    async fn unwired_actions_are_refused_with_a_reason() {
        let _guards = install();
        let (_server, backend) = fixture().await;
        let action = StateAction::TerminalInput(ahp_types::actions::TerminalInputAction {
            data: "ls\n".to_string(),
        });
        match backend.dispatch(&terminal::uri("t-1"), &action, &origin()) {
            DispatchOutcome::Rejected(reason) => {
                assert!(
                    reason.contains("terminal/input"),
                    "names the action: {reason}"
                );
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        uninstall();
    }
}
