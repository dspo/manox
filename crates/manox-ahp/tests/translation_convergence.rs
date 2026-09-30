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

/// An agentic tool loop: every assistant message is followed by a `stop` row
/// carrying the model's stop reason, and the turn only really ends at its
/// `turnFinish`. The stops are per-message metadata — no notice may surface
/// and no turn may close until the `turnFinish` arrives.
#[test]
fn a_stop_row_is_metadata_and_never_ends_the_turn() {
    let events = vec![
        Message {
            role: "user".to_string(),
            content: vec![serde_json::json!({"type": "text", "text": "look around"})],
            usage: None,
            origin_rpc: Some("rpc-1".to_string()),
            display: None,
        },
        AgentTextDelta {
            s: "Working.".to_string(),
        },
        Stop {
            reason: Some("tool_use".to_string()),
        },
        AgentTextDelta {
            s: "Still working.".to_string(),
        },
        Stop {
            reason: Some("end_turn".to_string()),
        },
        TurnFinish {
            cancelled: false,
            failed: false,
            stranded_steer_ids: Vec::new(),
        },
    ];
    let journal: Vec<JournalWireEntry> = events
        .into_iter()
        .enumerate()
        .map(|(seq, event)| JournalWireEntry {
            seq: seq as u64,
            id: format!("sentry-{seq}"),
            parent_id: (seq > 0).then(|| format!("sentry-{}", seq - 1)),
            timestamp: format!("2026-09-29T01:00:{seq:02}.000Z"),
            event,
        })
        .collect();

    let mut translator = Translator::new();
    let mut state = manox_ahp::channels::chat::initial("c-1");
    let mut stop_notes = 0usize;
    for entry in &journal {
        for emitted in translator.on_entry("c-1", "s-1", entry) {
            if let ahp_types::actions::StateAction::ChatResponsePart(part) = &emitted.action
                && matches!(part.part, ResponsePart::SystemNotification(_))
            {
                stop_notes += 1;
            }
            ahp::reducers::apply_action_to_chat(&mut state, &emitted.action);
        }
    }
    assert_eq!(
        stop_notes, 0,
        "a stop row is the model's own metadata, not a notice for the user"
    );
    assert_eq!(
        state.turns.len(),
        1,
        "the tool loop is one turn — the stops must not close it mid-round"
    );
    assert_eq!(
        state.turns.last().map(|t| t.message.text.as_str()),
        Some("look around"),
        "the turn's opening message is the submission"
    );
    assert!(
        state.active_turn.is_none(),
        "the turnFinish closed the turn"
    );
}

/// Journals written by older engine builds state the AskUserQuestion text in
/// `header` alone — the `question` field is a later schema addition (a device
/// journal carries exactly this shape). The elicitation must still carry the
/// structured questions; an unanswerable bare ask leaves the model waiting
/// forever.
#[test]
fn an_older_build_ask_payload_folds_its_questions_through_the_header() {
    let entry = JournalWireEntry {
        seq: 7,
        id: "e7".to_string(),
        parent_id: Some("e6".to_string()),
        timestamp: "2026-09-29T06:11:26.000Z".to_string(),
        event: Question {
            kind: "request".to_string(),
            auth_id: "call_00_LZg8".to_string(),
            tool_name: Some("AskUserQuestion".to_string()),
            tool_call_id: Some("call_00_LZg8".to_string()),
            verdict: None,
            reason: None,
            input: Some(serde_json::json!({
                "questions": [
                    {
                        "header": "交付方式",
                        "multiSelect": false,
                        "options": [
                            {"label": "发到 PR 评论区", "description": "gh pr comment"},
                            {"label": "贴回对话", "description": "直接回复"},
                        ],
                    }
                ],
            })),
        },
    };
    let mut translator = Translator::new();
    let actions: Vec<_> = translator
        .on_entry("c-1", "s-1", &entry)
        .into_iter()
        .map(|e| e.action)
        .collect();
    let request = actions.iter().find_map(|a| match a {
        ahp_types::actions::StateAction::ChatInputRequested(r) => Some(&r.request),
        _ => None,
    });
    let request = request.expect("the ask reaches the client as chat/inputRequested");
    assert_eq!(request.id, "call_00_LZg8");
    let questions = request.questions.as_ref().expect("structured questions");
    assert_eq!(
        questions.len(),
        1,
        "the v1 payload still yields its question"
    );
    let single = match &questions[0] {
        ahp_types::state::ChatInputQuestion::SingleSelect(s) => s,
        other => panic!("expected a single-select question, got {other:?}"),
    };
    assert_eq!(
        single.message, "交付方式",
        "the header IS the question text"
    );
    assert_eq!(single.options.len(), 2, "both options survive");
}

/// A question row carrying neither `question` nor `header` has nothing to
/// ask: the fold must degrade to the accepted bare ask (`questions: None`),
/// never to a silent textless card.
#[test]
fn an_ask_row_without_any_question_text_degrades_to_a_bare_ask() {
    let entry = JournalWireEntry {
        seq: 8,
        id: "e8".to_string(),
        parent_id: None,
        timestamp: "2026-09-29T06:20:00.000Z".to_string(),
        event: Question {
            kind: "request".to_string(),
            auth_id: "call_bare".to_string(),
            tool_name: Some("AskUserQuestion".to_string()),
            tool_call_id: None,
            verdict: None,
            reason: None,
            input: Some(serde_json::json!({
                "questions": [{"multiSelect": false, "options": []}],
            })),
        },
    };
    let mut translator = Translator::new();
    let actions: Vec<_> = translator
        .on_entry("c-1", "s-1", &entry)
        .into_iter()
        .map(|e| e.action)
        .collect();
    let request = actions.iter().find_map(|a| match a {
        ahp_types::actions::StateAction::ChatInputRequested(r) => Some(&r.request),
        _ => None,
    });
    let request = request.expect("the bare ask still reaches the client");
    assert_eq!(
        request.questions.as_ref(),
        None,
        "no text means no fabricated question card"
    );
}

/// A plan proposal reaches a subscribed client — content and question alike.
///
/// This is the one place the host emits an action AHP's own reducers do not know,
/// because `ChatInputRequest` has no `planReview` field in 0.9. An earlier
/// attempt put the whole card in a raw-JSON `chat/inputRequested`; it could not
/// work. `StateAction::Unknown` is an untagged serde fallback, so any parse
/// resolves the tag to the *typed* variant and drops the undeclared fields — and
/// the SDK client parses every inbound action, so the plan block was stripped on
/// arrival on both transports. The convergence gate cannot catch that (both ends
/// fold the raw action to nothing and agree), which is why the delivery needs an
/// assertion of its own.
///
/// The split asserted here is the fix: the plan's content rides the
/// `x-manox-plan` channel, whose payload survives because it is a *declared*
/// extension action rather than an unknown standard one, and the interactive
/// half is a real typed `chat/inputRequested` the host folds into the turn.
#[tokio::test(flavor = "multi_thread")]
async fn a_plan_proposal_reaches_a_subscribed_client() {
    let host = Host::new(TestBackend::new() as Arc<dyn Backend>);
    let (host_side, client_side) = manox_ahp::transport::inproc::pair();
    host.accept(host_side);
    let client = Client::connect(client_side, ClientConfig::default())
        .await
        .expect("client connects");
    client
        .initialize(
            "desktop".to_string(),
            vec![PROTOCOL_VERSION.to_string()],
            vec![TestBackend::chat_uri()],
        )
        .await
        .expect("initializes");
    let mut sub = client.attach_subscription(&TestBackend::chat_uri()).await;

    let mut translator = Translator::new();
    let mut published = 0usize;
    for (seq, event) in [
        TurnStart,
        PlanReview {
            state: "proposed".to_string(),
            plan_file: Some("/plans/demo-plan.md".to_string()),
            title: Some("Demo plan".to_string()),
            content: Some("# Demo\n\n- step one".to_string()),
            request_id: None,
        },
    ]
    .into_iter()
    .enumerate()
    {
        let entry = JournalWireEntry {
            seq: seq as u64 + 500,
            id: format!("pentry-{seq}"),
            parent_id: (seq > 0).then(|| format!("pentry-{}", seq - 1)),
            timestamp: format!("2026-09-23T02:00:{seq:02}.000Z"),
            event,
        };
        for emitted in translator.on_entry("c-1", "s-1", &entry) {
            published += 1;
            host.publish(&emitted.channel, emitted.action, None);
        }
    }
    assert!(published > 0, "the proposal must emit something");

    // Only the chat channel is subscribed here, so this is the question the
    // client is asked; the plan block is asserted through the host below.
    let mut card = None;
    for _ in 0..published {
        match tokio::time::timeout(Duration::from_secs(5), sub.recv()).await {
            Ok(Some(SubscriptionEvent::Action(envelope))) => {
                let value = serde_json::to_value(&envelope.action).expect("serializes");
                if value["type"] == "chat/inputRequested" {
                    card = Some(value);
                }
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }
    let card = card.expect("the plan question must reach a subscribed client");
    assert_eq!(
        card["request"]["id"], "plan-review:pentry-1",
        "the question is addressed by the id the runtime resolves: {card}"
    );
    assert_eq!(card["request"]["questions"][0]["kind"], "single-select");
    assert_eq!(
        card["request"]["questions"][0]["options"][0]["id"],
        "approve"
    );

    // The plan's content is on the extension channel, where the host folds it
    // and a reconnecting client is backfilled from.
    let plan = host
        .extension_state("x-manox-plan:/c-1")
        .expect("the plan channel holds the proposal");
    let plan = serde_json::to_value(&plan).expect("state serializes");
    let review = &plan["planReview"];
    assert_eq!(review["title"], "Demo plan", "held state is {plan}");
    assert_eq!(review["content"], "# Demo\n\n- step one");
    assert_eq!(review["actions"][0]["id"], "approve");
    assert_eq!(review["actions"][1]["id"], "refine");
    assert_eq!(review["canProvideFeedback"], true);
    assert!(
        review["planUri"]
            .as_str()
            .is_some_and(|u| u.contains("demo-plan")),
        "the block names the reviewed plan: {review}"
    );

    // …and the question is folded into the OPEN turn, not merely broadcast, so
    // the client can answer it through `chat/inputCompleted` and a reconnect is
    // backfilled from this state.
    let folded = host.chat_state("c-1").expect("host chat state");
    let active = folded
        .active_turn
        .as_ref()
        .expect("the proposal arrives while a turn is open");
    assert!(
        active
            .response_parts
            .iter()
            .any(|part| matches!(part, ResponsePart::InputRequest(_))),
        "the question must be folded into the turn: {:?}",
        active.response_parts
    );
}
