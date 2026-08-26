//! Full-workspace containment diagnostic for the message-list overlap bug.
//!
//! Rebuilds the production workspace from the real failing session
//! (`dfd73eed`) and walks the interaction matrix the live app exercises:
//! resize across the content-max threshold while parked at several scroll
//! offsets, then rebuild the conversation mid-scroll (thread switch /
//! `HistoryRestored` shape). Every message body that the walk's scroll matrix
//! brings on screen must stay inside its list row; an escape is the overlap
//! signature. Rows virtualized out of view are skipped, so coverage is bounded
//! by the scroll steps below — this is a local diagnostic harness, not a CI
//! regression guard.
//!
//! Compiled only under the `test-support` feature (the `Workspace::diagnostic_*`
//! hooks it uses are feature-gated), and a no-op when the local fixture is
//! absent, so CI and other machines stay green. Run locally via:
//! `cargo test -p agent-ui --features test-support --test workspace_overlap`.
//!
//! Lives in its own test binary because it initializes process-global
//! singletons (`agent::runtime`, `pi_providers`, `thread_store`) that cannot
//! coexist with other gpui tests in one process.
#![cfg(feature = "test-support")]

use std::{borrow::Cow, cell::RefCell, rc::Rc};

use agent_ui::Workspace;
use gpui::{AppContext as _, FollowMode, TestAppContext, VisualTestContext, px, size};
use gpui_component::Theme;

const FIXTURE: &str =
    "/Users/chenzhongrun/.manox/pi-sessions/dfd73eed-847d-4f42-97e5-72692ef39277.jsonl";

fn load_real_session_messages() -> Vec<agent::Message> {
    let source = std::fs::read_to_string(FIXTURE).expect("real session fixture");
    let harness_messages = source
        .lines()
        // The first 118 events cover the turns visible in the failure
        // screenshots (plan turn + numbered-list summary); truncation keeps the
        // walk light. Diagnostic-only: the bound is a perf choice, not coverage.
        .take(118)
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|event| event.get("type").and_then(|value| value.as_str()) == Some("message"))
        .filter_map(|event| event.get("message").cloned())
        .filter_map(|message| serde_json::from_value::<pi::types::AgentMessage>(message).ok())
        .collect::<Vec<_>>();
    agent::pi_engine::adapt::harness_messages_to_messages(&harness_messages)
}

fn register_lilex(cx: &mut TestAppContext) {
    cx.update(|cx| {
        cx.text_system()
            .add_fonts(vec![
                Cow::Borrowed(include_bytes!(
                    "../../manox/assets/fonts/lilex/Lilex-Light.ttf"
                )),
                Cow::Borrowed(include_bytes!(
                    "../../manox/assets/fonts/lilex/Lilex-Medium.ttf"
                )),
                Cow::Borrowed(include_bytes!(
                    "../../manox/assets/fonts/lilex/Lilex-LightItalic.ttf"
                )),
                Cow::Borrowed(include_bytes!(
                    "../../manox/assets/fonts/lilex/Lilex-MediumItalic.ttf"
                )),
            ])
            .expect("Lilex fonts");
        let theme = Theme::global_mut(cx);
        theme.mono_font_family = "Lilex".into();
        theme.mono_font_size = px(14.);
    });
}

fn assert_workspace_bodies_contained(
    visual: &mut VisualTestContext,
    item_count: usize,
    context: &str,
) {
    for ix in 0..item_count {
        let row_selector: &'static str =
            Box::leak(format!("workspace-message-row-{ix}").into_boxed_str());
        let body_selector: &'static str =
            Box::leak(format!("message-item-body-{ix}").into_boxed_str());
        let Some(row) = visual.debug_bounds(row_selector) else {
            continue;
        };
        let Some(body) = visual.debug_bounds(body_selector) else {
            continue;
        };
        assert!(
            body.top() >= row.top() - px(1.) && body.bottom() <= row.bottom() + px(1.),
            "[{context}] message body escaped row {ix}: row={row:?}, body={body:?}"
        );
    }
}

#[gpui::test]
async fn workspace_overlap_walk_scroll_resize_rebuild(cx: &mut TestAppContext) {
    if !std::path::Path::new(FIXTURE).exists() {
        return;
    }
    cx.update(gpui_component::init);
    register_lilex(cx);
    cx.update(|cx| {
        agent::runtime::init();
        agent::pi_providers::init();
        agent::thread_store::init(cx);
    });
    let messages = load_real_session_messages();
    let display: Vec<agent::db::HistoryEntry> = messages
        .iter()
        .cloned()
        .map(agent::db::HistoryEntry::Message)
        .collect();
    let weak = gpui::WeakEntity::<Workspace>::new_invalid();
    let conversation = cx.new(|cx| {
        agent_ui::conversation::ConversationState::rebuild_from_display(
            &display,
            &std::collections::HashMap::new(),
            "deepseek-v4-flash",
            true,
            agent_ui::conversation::ApplyCtx { weak, cwd: None },
            cx,
        )
    });
    let item_count = conversation.read_with(cx, |conversation, _cx| conversation.items().len());
    let workspace_cell: Rc<RefCell<Option<gpui::Entity<Workspace>>>> = Rc::new(RefCell::new(None));
    let capture = workspace_cell.clone();
    let window = cx.open_window(size(px(1_120.), px(780.)), {
        let conversation = conversation.clone();
        move |window, cx| {
            let workspace = cx.new(|cx| {
                let mut workspace = Workspace::new(window, cx);
                workspace.diagnostic_replace_conversation(conversation.clone(), cx);
                workspace
            });
            *capture.borrow_mut() = Some(workspace.clone());
            gpui_component::Root::new(workspace, window, cx)
        }
    });
    cx.run_until_parked();
    let mut visual = VisualTestContext::from_window(window.into(), cx);
    for _ in 0..3 {
        visual.update(|window, cx| window.draw(cx).clear());
    }
    assert_workspace_bodies_contained(&mut visual, item_count, "full workspace tail");

    // Walk the matrix the live app exercises: resize across the content-max
    // threshold while parked at several scroll offsets, then rebuild the
    // conversation mid-scroll (thread switch / HistoryRestored shape).
    let workspace = workspace_cell.borrow().clone().expect("workspace captured");
    let list_state =
        workspace.read_with(&visual.cx, |workspace, _| workspace.diagnostic_list_state());
    let draw = |visual: &mut VisualTestContext| {
        for _ in 0..2 {
            visual.update(|window, cx| window.draw(cx).clear());
        }
    };
    for width in [1_120., 1_700., 900., 1_500., 760., 1_400., 1_120.] {
        visual.simulate_resize(size(px(width), px(780.)));
        draw(&mut visual);
        assert_workspace_bodies_contained(&mut visual, item_count, &format!("w={width} tail"));
        for frac in [0.0_f32, 0.2, 0.45, 0.7] {
            let ix = (item_count as f32 * frac) as usize;
            list_state.set_follow_mode(FollowMode::Normal);
            list_state.scroll_to(gpui::ListOffset {
                item_ix: ix,
                offset_in_item: px(0.),
            });
            draw(&mut visual);
            assert_workspace_bodies_contained(
                &mut visual,
                item_count,
                &format!("w={width} ix={ix}"),
            );
        }
        list_state.set_follow_mode(FollowMode::Tail);
        draw(&mut visual);
        assert_workspace_bodies_contained(&mut visual, item_count, &format!("w={width} retail"));
    }
    list_state.set_follow_mode(FollowMode::Normal);
    list_state.scroll_to(gpui::ListOffset {
        item_ix: item_count / 3,
        offset_in_item: px(0.),
    });
    draw(&mut visual);
    let rebuilt = cx.new(|cx| {
        agent_ui::conversation::ConversationState::rebuild_from_display(
            &display,
            &std::collections::HashMap::new(),
            "deepseek-v4-flash",
            true,
            agent_ui::conversation::ApplyCtx {
                weak: gpui::WeakEntity::<Workspace>::new_invalid(),
                cwd: None,
            },
            cx,
        )
    });
    workspace.update(&mut visual.cx, |workspace, cx| {
        workspace.diagnostic_replace_conversation(rebuilt, cx);
    });
    draw(&mut visual);
    assert_workspace_bodies_contained(&mut visual, item_count, "rebuilt tail");
    list_state.scroll_to(gpui::ListOffset {
        item_ix: item_count / 4,
        offset_in_item: px(0.),
    });
    draw(&mut visual);
    assert_workspace_bodies_contained(&mut visual, item_count, "rebuilt quarter");
    agent::thread_store::drop_global_for_test();
}
