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

/// A mid-run steer is `Steering`, not `Queued` — and it is not published twice.
///
/// A manox steer is injected into the *running* turn and journalled as an
/// ordinary `user` row; it does not close the turn and does not wait for the
/// next one. AHP draws that distinction in `PendingMessageKind`: `Steering`
/// lands in `ChatState.steeringMessage` (consumed by the turn it interrupts),
/// `Queued` lands in `ChatState.queuedMessages` (carried into the *next*
/// turn). Translating a steer as `Queued` therefore does not merely mislabel it:
/// the row is still sitting in the queue when the next turn starts, so the same
/// text is injected a second time.
fn mid_run_steer_journal() -> Vec<JournalWireEntry> {
    let events = vec![
        // The turn the user is watching.
        Message {
            role: "user".to_string(),
            content: vec![serde_json::json!({"type": "text", "text": "run the tests"})],
            usage: None,
            origin_rpc: Some("rpc-submit".to_string()),
            display: None,
        },
        TurnStart,
        AgentTextDelta {
            s: "Starting.".to_string(),
        },
        // The steer: a `user` row that arrives with the turn still open. The
        // id is the client's steer id, which is also the id its
        // `chat/pendingMessageSet{kind: steering}` echo carried.
        Message {
            role: "user".to_string(),
            content: vec![serde_json::json!({"type": "text", "text": "use the fast suite"})],
            usage: None,
            origin_rpc: Some("steer-1".to_string()),
            display: None,
        },
        AgentTextDelta {
            s: "Using the fast suite.".to_string(),
        },
        TurnFinish {
            cancelled: false,
            failed: false,
            stranded_steer_ids: Vec::new(),
        },
    ];

    events
        .into_iter()
        .enumerate()
        .map(|(seq, event)| JournalWireEntry {
            seq: seq as u64,
            id: if seq == 3 {
                // The engine keys the injected row by the client's steer id.
                "steer-1".to_string()
            } else {
                format!("sentry-{seq}")
            },
            parent_id: (seq > 0).then(|| {
                if seq == 3 {
                    "sentry-2".to_string()
                } else {
                    format!("sentry-{}", seq - 1)
                }
            }),
            timestamp: format!("2026-09-23T01:00:{seq:02}.000Z"),
            event,
        })
        .collect()
}

/// The actions the translator emits for the mid-run steer scenario, reduced.
fn mid_run_steer_state() -> ChatState {
    let mut translator = Translator::new();
    let mut state = manox_ahp::channels::chat::initial("c-1");
    for entry in mid_run_steer_journal() {
        for emitted in translator.on_entry("c-1", "s-1", &entry) {
            ahp::reducers::apply_action_to_chat(&mut state, &emitted.action);
        }
    }
    state
}

#[test]
fn a_mid_run_steer_is_steering_not_queued() {
    let mut translator = Translator::new();
    let mut seen = Vec::new();
    for entry in mid_run_steer_journal() {
        for emitted in translator.on_entry("c-1", "s-1", &entry) {
            if let ahp_types::actions::StateAction::ChatPendingMessageSet(set) = &emitted.action {
                seen.push((format!("{:?}", set.kind), set.id.clone()));
            }
        }
    }
    // Two pending messages, and that is correct: the opening submission is a
    // genuine queue entry (the turn carries it as its first message), while the
    // steer is not. What must never happen is the steer appearing as a *second*
    // queued entry, or under a second id.
    let kinds: Vec<&str> = seen.iter().map(|(kind, _)| kind.as_str()).collect();
    assert_eq!(
        kinds,
        ["Queued", "Steering"],
        "the submission queues; the mid-run steer is injected, not queued (got {seen:?})"
    );
    let (_, steer_id) = &seen[1];
    assert_eq!(
        steer_id, "steer-1",
        "the pending id must be the client's steer id — the identity its echo \
         and the host's `steer` intent both use; a translator-minted id can be \
         retired by neither"
    );
    // The id is unique: a pending message the client cannot match against the
    // one it already holds is what produced two entries for one steer.
    let ids: Vec<&str> = seen.iter().map(|(_, id)| id.as_str()).collect();
    assert_eq!(
        ids.len(),
        ids.iter().collect::<std::collections::HashSet<_>>().len(),
        "pending ids must be distinct: {ids:?}"
    );
}

#[test]
fn a_consumed_steer_leaves_no_queued_residue() {
    let state = mid_run_steer_state();
    let queued = state.queued_messages.unwrap_or_default();
    assert!(
        queued.is_empty(),
        "the steer must not remain queued: a queued entry is re-injected as a \
         fresh user message by the next `turnStarted`, running the same text \
         twice (residue: {queued:?})"
    );
}

/// A later turn does not inherit the steer.
///
/// This is the consequence the kind fix exists for. The translator's contract
/// ends at "the steer is `Steering`, not `Queued`": it publishes no removal,
/// because the removal belongs to whoever observes the injection. What the fold
/// must show is that nothing carries the text forward — a `Queued` entry is
/// re-injected as a fresh user message by the next `turnStarted`.
///
/// The removal itself is asserted where it is emitted, on the host path
/// (`manox-ahp-runtime`'s dispatch), since this test drives the translator
/// alone and so never sees it.
#[test]
fn a_steer_is_not_replayed_by_a_later_turn() {
    let mut translator = Translator::new();
    let mut state = manox_ahp::channels::chat::initial("c-1");
    for entry in mid_run_steer_journal() {
        for emitted in translator.on_entry("c-1", "s-1", &entry) {
            ahp::reducers::apply_action_to_chat(&mut state, &emitted.action);
        }
    }

    // The steer reached the running turn, so the turn's opening message is
    // still the original submission — not the steer.
    let turn = state
        .turns
        .last()
        .expect("the scripted journal opens a turn");
    assert_eq!(
        turn.message.text, "run the tests",
        "the steer is injected into the running turn; it must not replace or \
         reopen it"
    );

    // Now the next turn starts. Nothing may carry the steer forward.
    let next = JournalWireEntry {
        seq: 99,
        id: "sentry-99".to_string(),
        parent_id: Some("sentry-5".to_string()),
        timestamp: "2026-09-23T01:01:00.000Z".to_string(),
        event: TurnStart,
    };
    for emitted in translator.on_entry("c-1", "s-1", &next) {
        ahp::reducers::apply_action_to_chat(&mut state, &emitted.action);
    }
    let queued = state.queued_messages.clone().unwrap_or_default();
    assert!(
        !queued.iter().any(|m| m.id == "steer-1"),
        "the next turn must not inherit the steer: {queued:?}"
    );
    // The steering slot itself is retired by the host's removal, not by the
    // fold — see the dispatch test. Assert it is still the CLIENT's id here, so
    // that removal has something to match.
    assert_eq!(
        state.steering_message.as_ref().map(|m| m.id.as_str()),
        Some("steer-1"),
        "the pending steering entry keeps the client's id, which is what the \
         host's removal matches on"
    );
}
