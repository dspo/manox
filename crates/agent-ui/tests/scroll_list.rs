//! Probe the message list's native `gpui::list` (index-anchored virtual list)
//! semantics: `ListAlignment::Bottom` gives chat-log layout — short histories
//! sit at the bottom of the viewport, long ones scroll, and `FollowMode::Tail`
//! re-pins to the end each layout while following. These tests pin the exact
//! nesting the production message column uses, so a regression in the list's
//! scroll anchoring fails here rather than as a blank screen.

use gpui::{
    AnyWindowHandle, AppContext as _, Context, FollowMode, InteractiveElement as _, IntoElement,
    ListAlignment, ListSizingBehavior, ListState, ParentElement as _, Pixels, Render, Styled as _,
    TestAppContext, Window, div, list, px,
};

struct NativeListProbe {
    state: ListState,
    body: Vec<Pixels>,
    viewport_h: Pixels,
}

impl Render for NativeListProbe {
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
                        list(state, move |ix, _, _| {
                            let height = body.get(ix).copied().unwrap_or(px(0.));
                            div()
                                .id(("native-c", ix))
                                .w(px(100.))
                                .h(height)
                                .flex_shrink_0()
                                .into_any_element()
                        })
                        .with_sizing_behavior(ListSizingBehavior::Auto)
                        .w_full()
                        .h_full()
                        .min_h_0(),
                    ),
            )
    }
}

fn draw_native_list(cx: &mut TestAppContext, body: Vec<Pixels>, viewport_h: Pixels) -> ListState {
    let state = ListState::new(body.len(), ListAlignment::Bottom, px(0.));
    let build = state.clone();
    let window = cx.add_window(move |_, _| NativeListProbe {
        state: build,
        body: body.clone(),
        viewport_h,
    });
    cx.run_until_parked();
    let any: AnyWindowHandle = window.into();
    cx.update_window(any, |_, window, cx| {
        window.draw(cx).clear();
    })
    .unwrap();
    state
}

/// Short content (fits the viewport) with `ListAlignment::Bottom`: the list
/// anchors at the tail — the last item sits at the bottom of the viewport,
/// matching chat-log semantics where the composer is below the last message.
#[gpui::test]
async fn native_bottom_list_anchors_short_content_in_h_flex_row(cx: &mut TestAppContext) {
    let state = draw_native_list(cx, vec![px(40.), px(40.)], px(100.));
    assert_eq!(
        state.logical_scroll_top().item_ix,
        state.item_count(),
        "ListAlignment::Bottom keeps a fitting chat list anchored at the tail"
    );
    assert_eq!(state.scroll_px_offset_for_scrollbar().y, px(0.));
}

/// Long content with `FollowMode::Tail`: the list re-anchors at the end
/// (`logical_scroll_top == item_count`) on each layout while following.
#[gpui::test]
async fn native_bottom_list_tail_follow_pins_end(cx: &mut TestAppContext) {
    let state = draw_native_list(cx, vec![px(40.); 4], px(100.));
    state.set_follow_mode(FollowMode::Tail);
    // Redraw so the list consumes the follow state and re-anchors at the end.
    let window = cx.add_window({
        let state = state.clone();
        move |_, _| NativeListProbe {
            state,
            body: vec![px(40.); 4],
            viewport_h: px(100.),
        }
    });
    cx.run_until_parked();
    let any: AnyWindowHandle = window.into();
    cx.update_window(any, |_, window, cx| {
        window.draw(cx).clear();
    })
    .unwrap();
    assert!(
        state.is_following_tail(),
        "FollowMode::Tail engages tail-follow"
    );
    assert_eq!(
        state.logical_scroll_top().item_ix,
        state.item_count(),
        "tail-follow anchors the logical scroll top past the last item"
    );
}
