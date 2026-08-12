//! Probe the message list's first-party `vlist` (index-anchored virtual list)
//! semantics under the exact nesting the production message column uses:
//! bottom alignment gives chat-log layout — short histories sit at the bottom
//! of the viewport, long ones scroll, and `FollowMode::Tail` re-pins to the
//! end each layout while following. A regression in the list's scroll
//! anchoring fails here rather than as a blank screen.

use agent_ui::views::vlist::{FollowMode, VListState, vlist};
use gpui::{
    AnyWindowHandle, AppContext as _, Context, InteractiveElement as _, IntoElement,
    ParentElement as _, Pixels, Render, Styled as _, TestAppContext, Window, div, px,
};

struct VlistProbe {
    state: VListState,
    body: Vec<Pixels>,
    viewport_h: Pixels,
}

impl Render for VlistProbe {
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
                        vlist(state, move |ix, _, _| {
                            let height = body.get(ix).copied().unwrap_or(px(0.));
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

fn draw_vlist(
    cx: &mut TestAppContext,
    body: Vec<Pixels>,
    viewport_h: Pixels,
) -> (AnyWindowHandle, VListState) {
    let state = VListState::new(body.len(), px(0.));
    let build = state.clone();
    let window = cx.add_window(move |_, _| VlistProbe {
        state: build,
        body: body.clone(),
        viewport_h,
    });
    let any: AnyWindowHandle = window.into();
    redraw(cx, any);
    (any, state)
}

fn redraw(cx: &mut TestAppContext, any: AnyWindowHandle) {
    cx.run_until_parked();
    cx.update_window(any, |_, window, cx| {
        window.draw(cx).clear();
    })
    .unwrap();
}

/// Short content (fits the viewport): nothing to scroll, every row measured,
/// and the scroll top stays at the content start while bottom alignment
/// places the rows at the viewport bottom (chat-log semantics, composer below
/// the last message).
#[gpui::test]
async fn vlist_bottom_anchors_short_content_in_h_flex_row(cx: &mut TestAppContext) {
    let (_window, state) = draw_vlist(cx, vec![px(40.), px(40.)], px(100.));
    let (scroll_top, scroll_max, total_h) = state.scroll_geometry();
    assert_eq!(total_h, px(80.), "both rows measured at the definite width");
    assert_eq!(scroll_max, px(0.), "fitting content is not scrollable");
    assert_eq!(scroll_top, px(0.));
}

/// Long content with `FollowMode::Tail`: the scroll top pins to the content
/// end on each layout while following.
#[gpui::test]
async fn vlist_tail_follow_pins_end(cx: &mut TestAppContext) {
    let (window, state) = draw_vlist(cx, vec![px(40.); 4], px(100.));
    state.set_follow_mode(FollowMode::Tail);
    // Redraw so the list consumes the follow state and re-anchors at the end.
    redraw(cx, window);
    assert!(
        state.is_following_tail(),
        "FollowMode::Tail engages tail-follow"
    );
    let (scroll_top, scroll_max, total_h) = state.scroll_geometry();
    assert_eq!(total_h, px(160.));
    assert_eq!(scroll_max, px(60.), "4×40 content in a 100px viewport");
    assert_eq!(
        scroll_top, scroll_max,
        "tail-follow pins the scroll top to the end"
    );
}
