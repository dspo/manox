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
fn install() -> (
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
fn uninstall() {
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
