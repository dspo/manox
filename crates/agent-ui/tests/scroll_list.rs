//! Probe the message list's native `gpui::list` semantics under the exact
//! nesting the production message column uses: bottom alignment gives chat-log
//! layout — short histories sit at the bottom of the viewport, long ones
//! scroll, and `FollowMode::Tail` re-pins to the end each layout while
//! following. A regression in the list's scroll

use std::{cell::RefCell, rc::Rc};

use agent_ui::{Workspace, conversation::ConvItem, views::message::MessageItem};
use gpui::{
    AnyWindowHandle, AppContext as _, Context, FollowMode, InteractiveElement as _, IntoElement,
    ListAlignment, ListState, ParentElement as _, Pixels, Render, Styled as _, TestAppContext,
    VisualTestContext, Window, WindowHandle, div, list, px, size,
};
use gpui_component::ActiveTheme as _;
use manox_components::markdown::{Markdown, PanelKind, TerminalPanel};

struct ListProbe {
    state: ListState,
    body: Rc<RefCell<Vec<Pixels>>>,
    viewport_h: Pixels,
}

impl Render for ListProbe {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let body = self.body.clone();
        let state = self.state.clone();
        div()
            .id("row")
            .w(px(100.))
            .h(self.viewport_h)
            .flex()
            .flex_row()
            .items_center()
            .child(
                div()
                    .id("wrap")
                    .flex_1()
                    .h_full()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(
                        list(state, move |ix, _window, _cx| {
                            let height = body.borrow().get(ix).copied().unwrap_or(px(0.));
                            div()
                                .id(("vc", ix))
                                .w(px(100.))
                                .h(height)
                                .flex_shrink_0()
                                .into_any_element()
                        })
                        .w_full()
                        .h_full()
                        .min_h_0(),
                    ),
            )
    }
}

fn draw_list(
    cx: &mut TestAppContext,
    body: Vec<Pixels>,
    viewport_h: Pixels,
) -> (WindowHandle<ListProbe>, Rc<RefCell<Vec<Pixels>>>, ListState) {
    let state = ListState::new(body.len(), ListAlignment::Bottom, px(2048.));
    let build = state.clone();
    let body = Rc::new(RefCell::new(body));
    let window = cx.add_window({
        let body = body.clone();
        let build = build.clone();
        move |_, _| ListProbe {
            state: build,
            body: body.clone(),
            viewport_h,
        }
    });
    redraw(cx, window.into());
    (window, body, state)
}

fn redraw(cx: &mut TestAppContext, any: AnyWindowHandle) {
    cx.run_until_parked();
    cx.update_window(any, |_, window, cx| {
        window.draw(cx).clear();
    })
    .unwrap();
}

/// Short content (fits the viewport): nothing to scroll, every row measured,
/// and the logical scroll top stays at the content end while bottom alignment
/// places the rows at the viewport bottom (chat-log semantics, composer below
/// the last message).
#[gpui::test]
async fn list_bottom_anchors_short_content_in_h_flex_row(cx: &mut TestAppContext) {
    let (_window, _body, state) = draw_list(cx, vec![px(40.), px(40.)], px(100.));
    let top = state.logical_scroll_top();
    assert_eq!(
        top.item_ix, 2,
        "Bottom alignment anchors at the content end"
    );
    assert_eq!(top.offset_in_item, px(0.));
    assert_eq!(
        state.is_scrolled_to_end(),
        None,
        "fitting content is not scrollable"
    );
}

/// Long content with `FollowMode::Tail`: the scroll top pins to the content
/// end on each layout while following.
#[gpui::test]
async fn list_tail_follow_pins_end(cx: &mut TestAppContext) {
    let (window, _body, state) = draw_list(cx, vec![px(40.); 4], px(100.));
    state.set_follow_mode(FollowMode::Tail);
    // Redraw so the list consumes the follow state and re-anchors at the end.
    redraw(cx, window.into());
    assert!(
        state.is_following_tail(),
        "FollowMode::Tail engages tail-follow"
    );
    assert_eq!(
        state.max_offset_for_scrollbar().y,
        px(60.),
        "4×40 content in a 100px viewport"
    );
    let top = state.logical_scroll_top();
    assert_eq!(top.item_ix, 4, "tail-follow pins the scroll top to the end");
    assert_eq!(top.offset_in_item, px(0.));
    assert_eq!(state.is_scrolled_to_end(), Some(true));
}

/// Regression: a row whose rendered height changes between frames without an
/// explicit `remeasure` call (async image load, font swap, lazy markdown
/// reflow) must still re-measure, else the cached height stays stale while the
/// painted element takes its fresh height — the element overflows its slot and
/// overlaps the next row ("weird display") or leaves a gap ("blank region").
/// The list's contract is that it re-measures every visible row each frame; a
/// `remeasure` call only widens the re-measurement to out-of-range rows.
#[gpui::test]
async fn list_remeasures_visible_rows_each_frame(cx: &mut TestAppContext) {
    let (window, body, state) = draw_list(cx, vec![px(40.), px(40.)], px(100.));
    assert_eq!(
        state.max_offset_for_scrollbar().y,
        px(0.),
        "both rows measured at the definite width; fitting content is not scrollable"
    );

    // Grow row 0 to 200px WITHOUT flagging `remeasure` — the production
    // paths that miss a remeasure look exactly like this to the list.
    body.borrow_mut()[0] = px(200.);
    redraw(cx, window.into());

    assert_eq!(
        state.max_offset_for_scrollbar().y,
        px(140.),
        "a visible row's height change must self-correct without remeasure"
    );
}

/// A visible row can shrink without an explicit list invalidation when a
/// nested entity collapses itself. The native list remeasures visible rows,
/// but it must also clamp a now-out-of-range intra-row scroll offset.
#[gpui::test]
async fn list_clamps_scroll_anchor_when_visible_row_shrinks(cx: &mut TestAppContext) {
    let (window, body, state) = draw_list(cx, vec![px(800.), px(40.), px(40.)], px(100.));
    state.set_follow_mode(FollowMode::Normal);
    state.scroll_to(gpui::ListOffset {
        item_ix: 0,
        offset_in_item: px(700.),
    });
    redraw(cx, window.into());
    assert_eq!(state.logical_scroll_top().item_ix, 0);
    assert_eq!(state.logical_scroll_top().offset_in_item, px(700.));

    // Model a nested collapse/finalize that only dirties the child entity.
    body.borrow_mut()[0] = px(80.);
    redraw(cx, window.into());

    let top = state.logical_scroll_top();
    assert!(
        top.item_ix > 0 || top.offset_in_item <= px(80.),
        "scroll anchor remained outside the shrunken row: {top:?}"
    );
}

struct MarkdownListProbe {
    state: ListState,
    rows: Vec<gpui::Entity<PersistentMarkdownRow>>,
}

struct PersistentMarkdownRow {
    markdown: gpui::Entity<Markdown>,
}

impl Render for PersistentMarkdownRow {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        agent_ui::views::centered(self.markdown.clone())
    }
}

impl Render for MarkdownListProbe {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.rows.clone();
        list(self.state.clone(), move |ix, _window, _cx| {
            div()
                .w_full()
                .min_w_0()
                .flex_shrink_0()
                .debug_selector(move || format!("markdown-list-row-{ix}"))
                .child(rows[ix].clone())
                .into_any_element()
        })
        .w_full()
        .h_full()
        .min_h_0()
        .min_w_0()
    }
}

/// Production-shaped regression: a persistent Markdown child must contribute
/// its wrapped height to the native list row. If intrinsic measurement treats
/// a transient zero width as an unwrapped line, the first row is far too short
/// and the second row is positioned over its painted text.
#[gpui::test]
async fn list_measures_persistent_wrapped_markdown_rows(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let long = (0..80)
        .map(|ix| format!("- item {ix}: 这是一段需要在确定宽度下换行的 markdown 内容。"))
        .collect::<Vec<_>>()
        .join("\n");
    let first_md = cx.new(|cx| Markdown::new("long-md", long).theme(cx.theme()));
    let second_md = cx.new(|cx| Markdown::new("tail-md", "tail message").theme(cx.theme()));
    let first = cx.new(|_| PersistentMarkdownRow { markdown: first_md });
    let second = cx.new(|_| PersistentMarkdownRow {
        markdown: second_md,
    });
    let state = ListState::new(2, ListAlignment::Bottom, px(2048.));
    state.set_follow_mode(FollowMode::Tail);
    let window = cx.open_window(size(px(760.), px(600.)), {
        let state = state.clone();
        move |_, _| MarkdownListProbe {
            state,
            rows: vec![first, second],
        }
    });
    cx.run_until_parked();
    let mut visual = VisualTestContext::from_window(window.into(), cx);
    visual.update(|window, cx| window.draw(cx).clear());

    let first = visual
        .debug_bounds("markdown-list-row-0")
        .expect("first row");
    let second = visual
        .debug_bounds("markdown-list-row-1")
        .expect("second row");
    assert!(
        first.size.height > px(600.),
        "long markdown was under-measured: {first:?}"
    );
    assert!(
        first.bottom() <= second.top(),
        "native list rows overlap: first={first:?}, second={second:?}"
    );
}

#[gpui::test]
async fn list_remeasures_streaming_markdown_growth_without_explicit_invalidation(
    cx: &mut TestAppContext,
) {
    cx.update(gpui_component::init);
    let prefix = "# Plan\n\nInitial paragraph.\n".to_string();
    let first_md = cx.new(|cx| {
        Markdown::new("streaming-plan", prefix.clone())
            .theme(cx.theme())
            .streaming(true)
    });
    let second_md = cx.new(|cx| Markdown::new("streaming-tail", "tool output").theme(cx.theme()));
    let first = cx.new(|_| PersistentMarkdownRow {
        markdown: first_md.clone(),
    });
    let second = cx.new(|_| PersistentMarkdownRow {
        markdown: second_md,
    });
    let state = ListState::new(2, ListAlignment::Bottom, px(2048.));
    state.set_follow_mode(FollowMode::Tail);
    let window = cx.open_window(size(px(760.), px(600.)), {
        let state = state.clone();
        let first_for_rows = first.clone();
        move |_, _| MarkdownListProbe {
            state,
            rows: vec![first_for_rows, second],
        }
    });
    cx.run_until_parked();
    let any = window.into();
    let mut visual = VisualTestContext::from_window(any, cx);
    visual.update(|window, cx| window.draw(cx).clear());

    let suffix = (0..100)
        .map(|ix| format!("\n## Section {ix}\n\n- streamed item with wrapped content {ix}\n"))
        .collect::<String>();
    first_md.update(&mut visual.cx, |md, cx| {
        md.replace(format!("{prefix}{suffix}"), cx)
    });
    visual.run_until_parked();
    visual.update(|window, cx| window.draw(cx).clear());

    let plan = visual
        .debug_bounds("markdown-list-row-0")
        .expect("streamed plan row");
    let tail = visual
        .debug_bounds("markdown-list-row-1")
        .expect("streamed tail row");
    assert!(
        plan.size.height > px(600.),
        "streamed plan was under-measured: {plan:?}"
    );
    assert!(
        plan.bottom() <= tail.top(),
        "streamed plan overlaps the next row: plan={plan:?}, tail={tail:?}"
    );
}

struct MessageItemListProbe {
    state: ListState,
    rows: Vec<gpui::Entity<MessageItem>>,
}

impl Render for MessageItemListProbe {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.rows.clone();
        list(self.state.clone(), move |ix, _window, _cx| {
            div()
                .w_full()
                .pt_1()
                .pb_4()
                .min_w_0()
                .flex_shrink_0()
                .debug_selector(move || format!("message-item-list-row-{ix}"))
                .child(rows[ix].clone())
                .into_any_element()
        })
        .w_full()
        .h_full()
        .min_h_0()
        .min_w_0()
    }
}

/// Exercise the actual production nesting (`list -> MessageItem -> Markdown`),
/// not just a fixed-height surrogate. Only the persistent Markdown child is
/// notified as streaming text grows; the list still has to rebuild the row
/// boundary before painting the following assistant message.
#[gpui::test]
async fn list_remeasures_real_message_item_when_markdown_child_grows(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let prefix = "# Plan\n\nInitial paragraph.\n".to_string();
    let weak = gpui::WeakEntity::<Workspace>::new_invalid();
    let plan = cx.new(|_| {
        MessageItem::new(
            ConvItem::Assistant {
                text: prefix.clone(),
                streaming: true,
                token_usage: None,
                activity_summary: None,
            },
            "DeepSeek".into(),
            0,
            weak.clone(),
        )
    });
    let tail = cx.new(|_| {
        MessageItem::new(
            ConvItem::Assistant {
                text: "tail message".into(),
                streaming: false,
                token_usage: None,
                activity_summary: None,
            },
            "DeepSeek".into(),
            1,
            weak,
        )
    });
    let state = ListState::new(2, ListAlignment::Bottom, px(2048.));
    state.set_follow_mode(FollowMode::Tail);
    let window = cx.open_window(size(px(760.), px(600.)), {
        let state = state.clone();
        let plan = plan.clone();
        move |_, _| MessageItemListProbe {
            state,
            rows: vec![plan, tail],
        }
    });
    cx.run_until_parked();
    let mut visual = VisualTestContext::from_window(window.into(), cx);
    visual.update(|window, cx| window.draw(cx).clear());

    let suffix = (0..100)
        .map(|ix| format!("\n## Section {ix}\n\n- streamed wrapped content for item {ix}\n"))
        .collect::<String>();
    let full = format!("{prefix}{suffix}");
    plan.update(&mut visual.cx, |item, cx| {
        if let ConvItem::Assistant { text, .. } = item.kind_mut() {
            *text = full.clone();
        }
        // `update_text` notifies the owned Markdown entity, deliberately not
        // the parent MessageItem or the list's offscreen height cache.
        item.update_text(&full, cx);
    });
    visual.run_until_parked();
    visual.update(|window, cx| window.draw(cx).clear());

    let plan = visual
        .debug_bounds("message-item-list-row-0")
        .expect("plan message row");
    let tail = visual
        .debug_bounds("message-item-list-row-1")
        .expect("tail message row");
    assert!(
        plan.size.height > px(600.),
        "plan row was under-measured: {plan:?}"
    );
    assert!(
        plan.bottom() <= tail.top(),
        "production MessageItem rows overlap: plan={plan:?}, tail={tail:?}"
    );
}

struct TerminalListProbe {
    state: ListState,
    panel: gpui::Entity<TerminalPanel>,
    tail: gpui::Entity<Markdown>,
}

impl Render for TerminalListProbe {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let panel = self.panel.clone();
        let tail = self.tail.clone();
        list(self.state.clone(), move |ix, _window, _cx| {
            div()
                .w_full()
                .min_w_0()
                .flex_shrink_0()
                .debug_selector(move || format!("terminal-list-row-{ix}"))
                .child(if ix == 0 {
                    panel.clone().into_any_element()
                } else {
                    tail.clone().into_any_element()
                })
                .into_any_element()
        })
        .w_full()
        .h_full()
        .min_h_0()
        .min_w_0()
    }
}

#[gpui::test]
async fn list_measures_persistent_terminal_panel_rows(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let output = (0..100)
        .map(|ix| format!("line {ix}: terminal output with enough text to wrap at the list width"))
        .collect::<Vec<_>>()
        .join("\n");
    let panel = cx.new(|cx| {
        let mut panel = TerminalPanel::new(PanelKind::Plain, None, None, cx.theme());
        panel.set_streaming(true, cx);
        panel.set_output(output, cx);
        panel
    });
    let tail = cx.new(|cx| Markdown::new("terminal-tail", "assistant tail").theme(cx.theme()));
    let state = ListState::new(2, ListAlignment::Bottom, px(2048.));
    state.set_follow_mode(FollowMode::Tail);
    let window = cx.open_window(size(px(760.), px(600.)), {
        let state = state.clone();
        move |_, _| TerminalListProbe { state, panel, tail }
    });
    cx.run_until_parked();
    let mut visual = VisualTestContext::from_window(window.into(), cx);
    visual.update(|window, cx| window.draw(cx).clear());

    let panel = visual
        .debug_bounds("terminal-list-row-0")
        .expect("panel row");
    let tail = visual
        .debug_bounds("terminal-list-row-1")
        .expect("tail row");
    assert!(
        panel.size.height > px(600.),
        "terminal panel was under-measured: {panel:?}"
    );
    assert!(
        panel.bottom() <= tail.top(),
        "terminal panel overlaps the next row: panel={panel:?}, tail={tail:?}"
    );
}
