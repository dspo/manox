//! Probe GPUI's `justify_end` + `overflow_y_scroll` interaction, and validate
//! the bottom-anchoring pattern used for the message list: an outer
//! `justify_end` flex column wrapping an inner `max_h_full` + `overflow_y_scroll`
//! list. Short content gets pushed to the bottom (next to the composer) while
//! overflowing content still scrolls normally without inverting offset semantics.

use gpui::{
    AnyWindowHandle, AppContext as _, Context, InteractiveElement, IntoElement, ParentElement,
    Pixels, Render, ScrollHandle, StatefulInteractiveElement, Styled, TestAppContext, Window, div,
    px,
};

struct Probe {
    handle: ScrollHandle,
    body: Vec<Pixels>,
    viewport_h: Pixels,
}

impl Render for Probe {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        // Outer flex column fills the viewport and bottom-aligns its child.
        // Inner list caps at the viewport height (`max_h_full`) but takes its
        // content height when shorter, so the outer justify_end can push it down.
        let items: Vec<_> = self
            .body
            .iter()
            .enumerate()
            .map(|(i, h)| div().id(("c", i)).w(px(100.)).h(*h).flex_shrink_0())
            .collect();
        let handle = self.handle.clone();
        div()
            .id("outer")
            .w(px(100.))
            .h(self.viewport_h)
            .flex()
            .flex_col()
            .justify_end()
            .child(
                div()
                    .id("list")
                    .w_full()
                    .max_h_full()
                    .min_h_0()
                    .flex_col()
                    .overflow_y_scroll()
                    .track_scroll(&handle)
                    .children(items),
            )
    }
}

fn draw(cx: &mut TestAppContext, body: Vec<Pixels>, viewport_h: Pixels) -> ScrollHandle {
    let handle = ScrollHandle::new();
    let build = handle.clone();
    let window = cx.add_window(move |_, _| Probe {
        handle: build,
        body: body.clone(),
        viewport_h,
    });
    cx.run_until_parked();
    let any: AnyWindowHandle = window.into();
    cx.update_window(any, |_, window, cx| {
        window.draw(cx).clear();
    })
    .unwrap();
    handle
}

#[gpui::test]
async fn pattern_anchors_short_content_to_bottom(cx: &mut TestAppContext) {
    // 2×40 = 80 < 100 viewport. The first item must sit at the bottom:
    // outer top is 0, list height = content = 80, justify_end pushes it to
    // y=20, so item 0 (first child of list) paints at y=20.
    let handle = draw(cx, vec![px(40.), px(40.)], px(100.));
    eprintln!(
        "short: list bounds={:?} item0={:?} off={:?} max={:?}",
        handle.bounds(),
        handle.bounds_for_item(0),
        handle.offset(),
        handle.max_offset()
    );
    let top = handle.bounds_for_item(0).map(|b| b.top()).unwrap_or(px(0.));
    let max = handle.max_offset().y;
    assert_eq!(top, px(20.), "short content must be pushed to the bottom");
    assert_eq!(max, px(0.), "no scroll range when content fits the viewport");
}

#[gpui::test]
async fn pattern_scrolls_long_content_without_inverting_offset(cx: &mut TestAppContext) {
    // 4×40 = 160 > 100 viewport. The list caps at 100 (max_h_full), content
    // overflows and scrolls. offset 0 must still show the TOP (item 0 at the
    // list's top, i.e. y=0) and max_offset must be 60.
    let handle = draw(cx, vec![px(40.); 4], px(100.));
    let top = handle.bounds_for_item(0).map(|b| b.top()).unwrap_or(px(0.));
    let off = handle.offset().y;
    let max = handle.max_offset().y;
    assert_eq!(max, px(60.), "overflow range = content - viewport = 60");
    assert_eq!(off, px(0.), "initial offset is 0 (top)");
    assert_eq!(top, px(0.), "item 0 at the top, not shifted past the viewport");
}

#[gpui::test]
async fn pattern_scroll_to_bottom_pins_tail(cx: &mut TestAppContext) {
    // Long content: scroll_to_bottom() must put offset at -max so the last
    // item's bottom lands at the viewport bottom (next to the composer).
    let handle = draw(cx, vec![px(40.); 4], px(100.));
    handle.scroll_to_bottom();
    let any = {
        // Redraw to consume the scroll_to_bottom flag in clamp_scroll_position.
        let window = cx.add_window(|_, _| Probe {
            handle: handle.clone(),
            body: vec![px(40.); 4],
            viewport_h: px(100.),
        });
        cx.run_until_parked();
        AnyWindowHandle::from(window)
    };
    cx.update_window(any, |_, window, cx| {
        window.draw(cx).clear();
    })
    .unwrap();
    let off = handle.offset().y;
    let max = handle.max_offset().y;
    assert_eq!(max, px(60.));
    assert_eq!(off, px(-60.), "scroll_to_bottom pins to -max_offset");
    // Last item's painted bottom = bounds.bottom() + offset.y = 160 + (-60) = 100 = viewport bottom.
    let last = handle.bounds_for_item(3).map(|b| b.bottom()).unwrap_or(px(0.));
    assert_eq!(
        last + off,
        px(100.),
        "last item bottom aligns with the viewport bottom"
    );
}

// Production nests the scroll column inside an `h_flex()` row, whose default
// `items_center` shrinks a cross-axis child to its content height. The
// wrapper's `h_full()` is what overrides that to fill the row vertically —
// drop it and `justify_end` has no room, so short content recenters instead of
// bottom-anchoring. This test pins the production nesting: removing `h_full()`
// from the wrapper regresses (item 0 lands at y=10, centered, not y=20).
#[gpui::test]
async fn pattern_fills_h_flex_row_and_bottom_anchors(cx: &mut TestAppContext) {
    let handle = draw_in_row(cx, vec![px(40.), px(40.)], px(100.));
    let top = handle.bounds_for_item(0).map(|b| b.top()).unwrap_or(px(0.));
    assert_eq!(
        top,
        px(20.),
        "wrapper h_full fills the items_center row so justify_end drops short content"
    );
}

struct RowProbe {
    handle: ScrollHandle,
    body: Vec<Pixels>,
    viewport_h: Pixels,
}

impl Render for RowProbe {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let items: Vec<_> = self
            .body
            .iter()
            .enumerate()
            .map(|(i, h)| div().id(("c", i)).w(px(100.)).h(*h).flex_shrink_0())
            .collect();
        let handle = self.handle.clone();
        // Mirrors production: an `h_flex()` row (flex + flex_row + items_center)
        // of definite height wrapping a `v_flex()` column carrying `flex_1`
        // (horizontal fill) + `h_full` (vertical fill, overrides items_center) +
        // `justify_end`, which wraps the `max_h_full` scroll list.
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
                    .justify_end()
                    .child(
                        div()
                            .id("list")
                            .w_full()
                            .max_h_full()
                            .min_h_0()
                            .flex()
                            .flex_col()
                            .overflow_y_scroll()
                            .track_scroll(&handle)
                            .children(items),
                    ),
            )
    }
}

fn draw_in_row(cx: &mut TestAppContext, body: Vec<Pixels>, viewport_h: Pixels) -> ScrollHandle {
    let handle = ScrollHandle::new();
    let build = handle.clone();
    let window = cx.add_window(move |_, _| RowProbe {
        handle: build,
        body: body.clone(),
        viewport_h,
    });
    cx.run_until_parked();
    let any: AnyWindowHandle = window.into();
    cx.update_window(any, |_, window, cx| {
        window.draw(cx).clear();
    })
    .unwrap();
    handle
}
