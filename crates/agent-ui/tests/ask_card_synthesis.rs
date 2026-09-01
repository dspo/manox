//! Regression: the AskUserQuestion card synthesized when the rebuilt
//! conversation lacks the top-level `ToolCall` item the interactive drawer
//! renders on. A parked interaction whose underlying ToolUse folded into an
//! activity segment leaves no card on a switch-back rebuild; the workspace
//! must synthesize the gate-created card.
#![cfg(feature = "test-support")]

mod common;

use agent_ui::Workspace;
use agent_ui::conversation::{ApplyCtx, ConversationState};
use common::{bash_tool_use_message, init_harness, open_workspace};
use gpui::{AppContext as _, TestAppContext, VisualTestContext};

#[gpui::test]
async fn ask_card_synthesized_when_rebuild_misses_the_tool_item(cx: &mut TestAppContext) {
    init_harness(cx);
    let (window, workspace) = open_workspace(cx);
    let mut visual = VisualTestContext::from_window(window.into(), cx);
    let payload = serde_json::json!({
        "questions": [
            {
                "question": "Which one?",
                "header": "Pick",
                "options": [
                    { "label": "A" },
                    { "label": "B" },
                ],
            }
        ]
    });

    // The conversation a switch-back rebuilds: a Bash ToolUse folded into a
    // Thinking segment, no top-level AskUserQuestion card.
    let weak = gpui::WeakEntity::<Workspace>::new_invalid();
    let conversation = cx.new(|cx| {
        ConversationState::rebuild_from_display(
            &[manox_agent::db::HistoryEntry::Message(
                bash_tool_use_message("t1"),
            )],
            &std::collections::HashMap::new(),
            "test-model",
            manox_agent::MessageAuthor::Lead,
            true,
            ApplyCtx { weak, cwd: None },
            cx,
        )
    });
    workspace.update(&mut visual.cx, |ws, cx| {
        ws.diagnostic_replace_conversation(conversation, cx);
    });
    assert_eq!(
        workspace.read_with(&visual.cx, |ws, cx| ws
            .diagnostic_tool_call_count("ask1", cx)),
        0,
        "rebuilt conversation must lack the top-level ask card"
    );

    // Re-surfacing the pending authorization: seed the ask and run the
    // synthesis the resurface loop performs per gate entry.
    workspace.update(&mut visual.cx, |ws, cx| {
        ws.diagnostic_seed_ask("ask1", payload.clone(), cx);
        ws.diagnostic_ensure_ask_tool_item("ask1", "summary", payload.clone(), cx);
    });
    workspace.update(&mut visual.cx, |ws, cx| {
        ws.diagnostic_sync_ask_card_snapshots(cx);
    });

    let interactive = workspace.read_with(&visual.cx, |ws, cx| {
        ws.diagnostic_ask_card_interactive("ask1", cx)
    });
    assert!(interactive, "synthesized ask card must be interactive");
    let cards = workspace.read_with(&visual.cx, |ws, cx| {
        ws.diagnostic_tool_call_count("ask1", cx)
    });
    assert_eq!(cards, 1, "exactly one ask card");
    manox_agent::thread_store::drop_global_for_test();
}
