//! The convergence gate: what the host folds and what a client folds from the
//! same envelopes must be the same state.
//!
//! AHP's write-ahead reconciliation only works because both ends run the same
//! reducers over the same ordered action stream. This suite drives a scripted
//! journal through the translator, publishes every produced action on a host
//! whose chat/session channels a real SDK client is subscribed to, reduces the
//! envelopes the client actually received, and asserts the result equals the
//! host's own state. A translation that silently drops, reorders or misplaces an
//! action therefore fails here instead of diverging in a user's window.

mod common;

use std::sync::Arc;
use std::time::Duration;

use ahp::{Client, ClientConfig, SubscriptionEvent};
use ahp_types::actions::ActionEnvelope;
use ahp_types::state::{ChatState, ResponsePart, SnapshotState, ToolCallState, TurnState};
use ahp_types::version::PROTOCOL_VERSION;
use manox_ahp::Host;
use manox_ahp::backend::Backend;
use manox_ahp::translate::Translator;
use manox_journal::JournalWireEvent::*;
use manox_journal::{JournalWireEntry, UsagePayload};

use common::TestBackend;

/// One scripted turn plus the session/plan state changes that follow it.
fn scripted_journal() -> Vec<JournalWireEntry> {
    let events = vec![
        Message {
            role: "user".to_string(),
            content: vec![serde_json::json!({"type": "text", "text": "run the tests"})],
            usage: None,
            origin_rpc: Some("rpc-1".to_string()),
            display: None,
        },
        TurnStart,
        AgentTextDelta {
            s: "Running ".to_string(),
        },
        AgentTextDelta {
            s: "them now.".to_string(),
        },
        AgentThinkingDelta {
            s: "which suite?".to_string(),
        },
        ToolCall {
            call_id: "call-1".to_string(),
            name: "bash".to_string(),
            title: "cargo test".to_string(),
            status: "running".to_string(),
            input: serde_json::json!({"command": "cargo test"}),
        },
        Approval {
            kind: "request".to_string(),
            auth_id: "auth-1".to_string(),
            tool_name: Some("bash".to_string()),
            tool_call_id: Some("call-1".to_string()),
            verdict: None,
            reason: None,
        },
        Approval {
            kind: "decision".to_string(),
            auth_id: "auth-1".to_string(),
            tool_name: Some("bash".to_string()),
            tool_call_id: Some("call-1".to_string()),
            verdict: Some("allow_once".to_string()),
            reason: None,
        },
        ToolResult {
            call_id: "call-1".to_string(),
            output: "ok 24 passed".to_string(),
            is_error: false,
        },
        Message {
            role: "assistant".to_string(),
            content: vec![serde_json::json!({"type": "text", "text": "all green"})],
            usage: Some(UsagePayload {
                input: 120,
                output: 40,
                cache_read: 10,
                cache_write: 5,
                reasoning: 7,
            }),
            origin_rpc: None,
            display: None,
        },
        TurnFinish {
            cancelled: false,
            failed: false,
            stranded_steer_ids: Vec::new(),
        },
        Title {
            title: "scripted conversation".to_string(),
        },
        PlanModeChange { enabled: true },
    ];

    events
        .into_iter()
        .enumerate()
        .map(|(seq, event)| JournalWireEntry {
            seq: seq as u64,
            id: format!("entry-{seq}"),
            parent_id: (seq > 0).then(|| format!("entry-{}", seq - 1)),
            timestamp: format!("2026-09-23T00:00:{seq:02}.000Z"),
            event,
        })
        .collect()
}

/// Reduce `envelopes` into the chat `chat_id` starting from `seed` — exactly
/// what a client does with what it receives.
fn reduce_chat(seed: &ChatState, envelopes: &[ActionEnvelope]) -> ChatState {
    let mut state = seed.clone();
    for envelope in envelopes {
        ahp::reducers::apply_action_to_chat(&mut state, &envelope.action);
    }
    state
}

/// Reduce `envelopes` into the session `seed`.
fn reduce_session(
    seed: &ahp_types::state::SessionState,
    envelopes: &[ActionEnvelope],
) -> ahp_types::state::SessionState {
    let mut state = seed.clone();
    for envelope in envelopes {
        ahp::reducers::apply_action_to_session(&mut state, &envelope.action);
    }
    state
}

#[tokio::test(flavor = "multi_thread")]
async fn client_fold_equals_host_fold_for_the_whole_scripted_turn() {
    let host = Host::new(TestBackend::new() as Arc<dyn Backend>);
    let (host_side, client_side) = manox_ahp::transport::inproc::pair();
    host.accept(host_side);
    let client = Client::connect(client_side, ClientConfig::default())
        .await
        .expect("connects");
    client
        .initialize(
            "gate".to_string(),
            vec![PROTOCOL_VERSION.to_string()],
            vec![],
        )
        .await
        .expect("initializes");

    let (chat_subscribe, mut chat_sub) = client
        .subscribe(TestBackend::chat_uri())
        .await
        .expect("subscribes to the chat");
    let (session_subscribe, mut session_sub) = client
        .subscribe(TestBackend::session_uri())
        .await
        .expect("subscribes to the session");

    let chat_seed = match chat_subscribe.snapshot.expect("chats carry state").state {
        SnapshotState::Chat(state) => *state,
        other => panic!("expected chat state, got {other:?}"),
    };
    let session_seed = match session_subscribe
        .snapshot
        .expect("sessions carry state")
        .state
    {
        SnapshotState::Session(state) => *state,
        other => panic!("expected session state, got {other:?}"),
    };

    let mut translator = Translator::new();
    let mut published_on_chat = 0usize;
    let mut published_on_session = 0usize;
    for entry in scripted_journal() {
        for emitted in translator.on_entry("c-1", "s-1", &entry) {
            match emitted.channel.as_str() {
                c if c == TestBackend::chat_uri() => published_on_chat += 1,
                c if c == TestBackend::session_uri() => published_on_session += 1,
                _ => {}
            }
            host.publish(&emitted.channel, emitted.action, None);
        }
    }
    assert!(
        published_on_chat >= 8,
        "the scripted turn must produce a real action stream, got {published_on_chat}"
    );
    assert!(
        published_on_session >= 1,
        "the title change must land on the session channel"
    );

    let chat_envelopes = drain(&mut chat_sub, published_on_chat).await;
    let session_envelopes = drain(&mut session_sub, published_on_session).await;

    // ① The load-bearing assertion: same envelopes, same state.
    let host_chat = host.chat_state("c-1").expect("host chat state");
    let client_chat = reduce_chat(&chat_seed, &chat_envelopes);
    assert_eq!(host_chat, client_chat, "chat fold diverged");
    let host_session = host.session_state("s-1").expect("host session state");
    let client_session = reduce_session(&session_seed, &session_envelopes);
    assert_eq!(host_session, client_session, "session fold diverged");

    // ② …and the converged state is the turn the journal described, so an
    // "everything reduced to a no-op" translation cannot pass ①.
    assert_eq!(host_chat.turns.len(), 1, "one completed turn");
    let turn = &host_chat.turns[0];
    assert_eq!(turn.state, TurnState::Complete);
    assert_eq!(turn.message.text, "run the tests");
    assert!(
        turn.usage.is_some(),
        "the assistant row's usage is reported"
    );

    let markdown: String = turn
        .response_parts
        .iter()
        .filter_map(|part| match part {
            ResponsePart::Markdown(part) => Some(part.content.clone()),
            _ => None,
        })
        .collect();
    assert!(
        markdown.contains("Running") && markdown.contains("them now."),
        "both deltas append into the transcript, got {markdown:?}"
    );
    assert!(
        turn.response_parts
            .iter()
            .any(|part| matches!(part, ResponsePart::Reasoning(_))),
        "the thinking delta keeps a reasoning part"
    );
    let tool_call = turn
        .response_parts
        .iter()
        .find_map(|part| match part {
            ResponsePart::ToolCall(part) => Some(&part.tool_call),
            _ => None,
        })
        .expect("the tool call is in the transcript");
    match tool_call {
        ToolCallState::Completed(completed) => {
            assert!(completed.success, "the settled call reports success");
        }
        other => panic!("the approval decision and the result close the call, got {other:?}"),
    }

    assert_eq!(host_session.title, "scripted conversation");
}

/// Collect exactly `expected` action envelopes for the subscription, failing
/// loudly rather than hanging when the host under-delivers.
async fn drain(sub: &mut ahp::SessionSubscription, expected: usize) -> Vec<ActionEnvelope> {
    let mut out = Vec::new();
    while out.len() < expected {
        match tokio::time::timeout(Duration::from_secs(5), sub.recv()).await {
            Ok(Some(SubscriptionEvent::Action(envelope))) => out.push(envelope),
            Ok(Some(other)) => panic!("unexpected subscription event: {other:?}"),
            Ok(None) => break,
            Err(_) => panic!("timed out after {} of {expected} envelopes", out.len()),
        }
    }
    assert_eq!(
        out.len(),
        expected,
        "envelope count delivered to the client"
    );
    out
}

/// Compile-time reminder that the translation must stay total over the journal
/// vocabulary: every variant is matched by `target_of`, and this list is what
/// the coverage test walks when a kernel kind is added.
#[test]
fn scripted_journal_covers_the_groups_the_gate_cares_about() {
    let tags: Vec<String> = scripted_journal()
        .iter()
        .map(|entry| {
            serde_json::to_value(&entry.event)
                .expect("events serialize")
                .get("type")
                .and_then(|value| value.as_str())
                .expect("tagged")
                .to_string()
        })
        .collect();
    for required in [
        "message",
        "turnStart",
        "agentTextDelta",
        "agentThinkingDelta",
        "toolCall",
        "approval",
        "toolResult",
        "turnFinish",
        "title",
        "planModeChange",
    ] {
        assert!(tags.iter().any(|tag| tag == required), "missing {required}");
    }
}
