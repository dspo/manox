//! Repro tests for the two-concurrent-threads crash. Both are `#[ignore]`-gated:
//! they need the real provider config at `~/.config/cx/` and (for the live
//! variant) `MANOX_RUN_LIVE=1`. Run with `cargo test -p agent-ui -- --ignored`.

#![cfg(all(test, feature = "debug"))]

use std::time::Duration;

use agent::language_model::{LanguageModelCompletionEvent, StopReason};
use agent::{ThreadEvent, ToolCallStatus};
use serde_json::json;

use super::test_support::{ReplayModel, build_workspace, setup};
use super::{Harness, IdleState, await_idle_sync};

#[gpui::test]
async fn direct_messages_route_pending_plan_and_ask_inline(cx: &mut gpui::TestAppContext) {
    let guard = setup(cx);
    let model = ReplayModel::build("test/inline-interactions", Vec::new());
    let ws = build_workspace(cx, model);
    let h = Harness::new(ws.clone());

    cx.update(|cx| {
        let thread = ws.read(cx).thread.clone();
        thread.update(cx, |_, cx| {
            cx.emit(ThreadEvent::ToolCall {
                id: "plan_1".to_string(),
                name: "exit_plan_mode".to_string(),
                title: "Submit plan".to_string(),
                status: ToolCallStatus::PendingApproval,
            });
            cx.emit(ThreadEvent::PlanProposed {
                id: "plan_1".to_string(),
                plan_text: "## Plan\n\nKeep this plan visible.".to_string(),
            });
        });
    });
    assert_eq!(
        cx.update(|cx| h.pending_plan_id(cx)),
        Some("plan_1".to_string())
    );

    let conversation = cx.update(|cx| h.read_conversation(cx));
    let plan_card = conversation["items"]
        .as_array()
        .and_then(|items| {
            items.iter().find_map(|item| {
                (item["kind"] == "tool_call" && item["name"] == "exit_plan_mode").then_some(item)
            })
        })
        .expect("plan card is in the conversation");
    assert_eq!(plan_card["status"], "PendingApproval");
    assert_eq!(plan_card["output"], "## Plan\n\nKeep this plan visible.");

    cx.update(|cx| {
        h.send_message("Please revise the plan with the new scope.".into(), cx)
            .unwrap()
    });
    assert_eq!(cx.update(|cx| h.pending_plan_id(cx)), None);
    assert!(
        cx.update(|cx| h.has_deferred_plan_turn(cx)),
        "composer submit should defer the user turn until the plan-continue result lands"
    );

    let ask_input = json!({
        "questions": [{
            "header": "Scope",
            "question": "Which path should we take?",
            "options": [
                { "label": "A", "description": "First path" },
                { "label": "B", "description": "Second path" }
            ]
        }]
    });
    cx.update(|cx| {
        let thread = ws.read(cx).thread.clone();
        thread.update(cx, |_, cx| {
            cx.emit(ThreadEvent::ToolCallAuthorization {
                id: "ask_1".to_string(),
                tool_name: "AskUserQuestion".to_string(),
                summary: "Which path should we take?".to_string(),
                input: ask_input,
            });
        });
    });
    assert!(cx.update(|cx| h.has_pending_ask(cx)));
    assert_eq!(cx.update(|cx| h.pending_auth_count(cx)), 1);

    cx.update(|cx| h.send_message("Use the second path.".into(), cx).unwrap());
    assert!(
        !cx.update(|cx| h.has_pending_ask(cx)),
        "composer submit should resolve the inline ask card"
    );
    assert_eq!(cx.update(|cx| h.pending_auth_count(cx)), 0);
    let conversation = cx.update(|cx| h.read_conversation(cx));
    let user_bubbles = conversation["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["kind"] == "user")
        .count();
    assert_eq!(
        user_bubbles, 0,
        "ask response is not a duplicate user bubble"
    );

    drop(guard);
}

/// Offline: two workspaces, two concurrent `run_turn`s against a replay model.
/// Reproduces the gpui-side concurrency path (double-lease, entity racing)
/// without a live provider. `#[ignore]` because `setup()` reads the real
/// provider config to satisfy `Workspace::new`'s registry lookup.
#[gpui::test]
#[ignore = "reads ~/.config/cx provider config; run with --ignored"]
async fn two_concurrent_threads_repro(cx: &mut gpui::TestAppContext) {
    let _guard = setup(cx);
    let model = ReplayModel::build(
        "test/replay",
        vec![
            LanguageModelCompletionEvent::Text("hello from model".into()),
            LanguageModelCompletionEvent::Stop(StopReason::EndTurn),
        ],
    );
    let ws1 = build_workspace(cx, model.clone());
    let ws2 = build_workspace(cx, model);

    let h1 = Harness::new(ws1.clone());
    let h2 = Harness::new(ws2.clone());
    cx.update(|cx| {
        h1.send_message("msg1".into(), cx).unwrap();
        h2.send_message("msg2".into(), cx).unwrap();
    });

    let s1 = await_idle_sync(&ws1, cx, Duration::from_secs(30));
    let s2 = await_idle_sync(&ws2, cx, Duration::from_secs(30));
    assert_ne!(s1, IdleState::StillRunning, "ws1 never went idle");
    assert_ne!(s2, IdleState::StillRunning, "ws2 never went idle");
    drop(_guard);
}

/// Live: real Bailian glm-5.2, two concurrent turns — the exact SIGABRT
/// scenario the user reported. Gated by `MANOX_RUN_LIVE`.
#[gpui::test]
#[ignore = "live provider + MANOX_RUN_LIVE required; reproduces the SIGABRT scenario"]
async fn two_concurrent_threads_live(cx: &mut gpui::TestAppContext) {
    if std::env::var("MANOX_RUN_LIVE").is_err() {
        return;
    }
    let guard = setup(cx);
    let model = cx.update(|_cx| {
        agent::provider::registry::global()
            .models()
            .iter()
            .find(|m| m.name().contains("glm-5.2"))
            .cloned()
            .expect("glm-5.2 model in registry")
    });
    let ws1 = build_workspace(cx, model.clone());
    let ws2 = build_workspace(cx, model);

    let h1 = Harness::new(ws1.clone());
    let h2 = Harness::new(ws2.clone());
    cx.update(|cx| {
        h1.send_message("杭州最近天气".into(), cx).unwrap();
        h2.send_message("北京最近天气".into(), cx).unwrap();
    });

    let s1 = await_idle_sync(&ws1, cx, Duration::from_secs(90));
    let s2 = await_idle_sync(&ws2, cx, Duration::from_secs(90));
    assert_ne!(s1, IdleState::StillRunning, "ws1 live turn did not finish");
    assert_ne!(s2, IdleState::StillRunning, "ws2 live turn did not finish");
    drop(guard);
}
