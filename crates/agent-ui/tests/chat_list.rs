//! Integration tests for the anchor-only `ChatList` under the exact nesting
//! the production message column uses.
//!
//! The four headline cases are the ones a cached-height virtual list can never
//! satisfy, no matter how its invalidation is wired:
//! 1. growing an offscreen row must not shift the anchored row;
//! 2. every row measurement happens at a definite width;
//! 3. a row painting taller than it measures must leave its neighbours alone;
//! 4. a width change needs no invalidation and no extra frame.

use std::{cell::RefCell, rc::Rc};

use gpui::{
    App, AppContext as _, Context, Element, ElementId, GlobalElementId, InputEvent as _,
    InspectorElementId, InteractiveElement as _, IntoElement, LayoutId, ParentElement as _, Pixels,
    Render, ScrollDelta, ScrollWheelEvent, Style, Styled as _, TestAppContext, TouchPhase,
    VisualTestContext, Window, div, point, px, size,
};
use manox_components::chat_list::{ChatList, ChatListState, RowKey};

#[derive(Default)]
struct ProbeConfig {
    keys: Vec<RowKey>,
    recorder: Option<Rc<RefCell<Vec<gpui::AvailableSpace>>>>,
    malicious: Option<usize>,
    renders: Rc<RefCell<usize>>,
}

struct ListProbe {
    state: ChatListState,
    body: Rc<RefCell<Vec<Pixels>>>,
    viewport_h: Pixels,
    config: Rc<RefCell<ProbeConfig>>,
}

impl Render for ListProbe {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let body = self.body.clone();
        let state = self.state.clone();
        let config = self.config.clone();
        let keys = config.borrow().keys.clone();
        div()
            .id("list-probe")
            .w(px(480.))
            .h(self.viewport_h)
            .flex()
            .flex_col()
            .child(
                ChatList::new(state, keys, move |ix, _window, _cx| {
                    *config.borrow().renders.borrow_mut() += 1;
                    let height = body.borrow().get(ix).copied().unwrap_or(px(0.));
                    let config = config.borrow();
                    let mut row = div()
                        .debug_selector(move || format!("chat-row-{ix}"))
                        .w(px(480.))
                        .h(height)
                        .flex_shrink_0();
                    if config.malicious == Some(ix) {
                        // Reports 40px, paints 400px: overflow must be
                        // clipped to its own slot rather than pushing the
                        // next row.
                        row = row.child(div().h(px(400.)));
                    }
                    if let Some(widths) = config.recorder.as_ref() {
                        row = row.child(WidthRecorder {
                            widths: widths.clone(),
                        });
                    }
                    row.into_any_element()
                })
                .w_full()
                .h_full(),
            )
    }
}

/// A leaf whose measured-layout callback records every width constraint it saw.
#[derive(Clone)]
struct WidthRecorder {
    widths: Rc<RefCell<Vec<gpui::AvailableSpace>>>,
}

impl IntoElement for WidthRecorder {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for WidthRecorder {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        _cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let widths = self.widths.clone();
        let layout_id =
            window.request_measured_layout(Style::default(), move |_known, available, _w, _cx| {
                widths.borrow_mut().push(available.width);
                size(px(0.), px(0.))
            });
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: gpui::Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        _window: &mut Window,
        _cx: &mut App,
    ) {
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: gpui::Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        _: &mut Self::PrepaintState,
        _window: &mut Window,
        _cx: &mut App,
    ) {
    }
}

fn keys(n: usize) -> Vec<RowKey> {
    (0..n)
        .map(|i| RowKey::from_entity_id(gpui::EntityId::from(i as u64)))
        .collect()
}

struct Draw {
    window: gpui::AnyWindowHandle,
    body: Rc<RefCell<Vec<Pixels>>>,
    config: Rc<RefCell<ProbeConfig>>,
    state: ChatListState,
}

fn draw_list(cx: &mut TestAppContext, body: Vec<Pixels>, viewport_h: f32) -> Draw {
    let state = ChatListState::bottom_aligned();
    let body = Rc::new(RefCell::new(body));
    let config = Rc::new(RefCell::new(ProbeConfig::default()));
    let window = cx.open_window(size(px(480.), px(viewport_h)), {
        let state = state.clone();
        let body = body.clone();
        let config = config.clone();
        move |_, _| ListProbe {
            state,
            body: body.clone(),
            viewport_h: px(viewport_h),
            config: config.clone(),
        }
    });
    let draw = Draw {
        window: window.into(),
        body,
        config,
        state,
    };
    redraw(cx, draw.window);
    draw
}

fn redraw(cx: &mut TestAppContext, window: gpui::AnyWindowHandle) {
    cx.run_until_parked();
    let mut visual = VisualTestContext::from_window(window, cx);
    visual.update(|window, cx| window.draw(cx).clear());
}

fn simulate_scroll(cx: &mut TestAppContext, window: gpui::AnyWindowHandle, delta: ScrollDelta) {
    let event = ScrollWheelEvent {
        position: point(px(100.), px(50.)),
        delta,
        modifiers: Default::default(),
        touch_phase: TouchPhase::Moved,
    };
    cx.update_window(window, |_, window, cx| {
        window.dispatch_event(event.to_platform_input(), cx);
    })
    .unwrap();
    redraw(cx, window);
}

/// Growing an offscreen row must not shift the anchored row: the anchor is the
/// only cross-frame state, so a height change before the anchor is irrelevant
/// until the user scrolls there.
#[gpui::test]
async fn growing_an_offscreen_row_does_not_shift_the_anchored_row(cx: &mut TestAppContext) {
    let draw = draw_list(cx, vec![px(20.); 100], 300.);
    draw.config.borrow_mut().keys = keys(100);
    draw.state
        .scroll_to_row(RowKey::from_entity_id(gpui::EntityId::from(50)));
    redraw(cx, draw.window);

    let mut visual = VisualTestContext::from_window(draw.window, cx);
    let before = visual
        .debug_bounds("chat-row-50")
        .expect("anchored row placed");
    assert_eq!(
        before.top(),
        px(0.),
        "scroll_to_row pins the row to the top"
    );
    drop(visual);

    // Row 3 is far offscreen above the anchor. Growing it must not move row 50.
    draw.body.borrow_mut()[3] = px(10_000.);
    redraw(cx, draw.window);

    let mut visual = VisualTestContext::from_window(draw.window, cx);
    let after = visual
        .debug_bounds("chat-row-50")
        .expect("anchored row still placed");
    assert_eq!(
        after.top(),
        before.top(),
        "offscreen growth must not shift the anchor"
    );
}

/// Every row measurement happens at a definite width — a `MinContent` probe is
/// exactly the class of constraint that poisoned the old `StyledText` cache.
#[gpui::test]
async fn rows_are_always_measured_at_a_definite_width(cx: &mut TestAppContext) {
    let draw = draw_list(cx, vec![px(20.); 100], 300.);
    let widths = Rc::new(RefCell::new(Vec::new()));
    draw.config.borrow_mut().keys = keys(100);
    draw.config.borrow_mut().recorder = Some(widths.clone());
    redraw(cx, draw.window);

    let widths = widths.borrow();
    assert!(!widths.is_empty(), "rows were measured");
    for w in widths.iter() {
        assert!(
            matches!(w, gpui::AvailableSpace::Definite(_)),
            "row measured at a non-definite width: {w:?}"
        );
    }
}

/// A row that paints taller than it reports leaves its neighbours untouched:
/// the per-row content mask clips the overflow into its own slot.
#[gpui::test]
async fn malicious_row_painting_taller_than_it_measures_leaves_neighbours_untouched(
    cx: &mut TestAppContext,
) {
    let draw = draw_list(cx, vec![px(40.); 5], 300.);
    draw.config.borrow_mut().keys = keys(5);
    draw.config.borrow_mut().malicious = Some(1);
    redraw(cx, draw.window);

    let mut visual = VisualTestContext::from_window(draw.window, cx);
    let r0 = visual.debug_bounds("chat-row-0").expect("row 0");
    let r1 = visual.debug_bounds("chat-row-1").expect("row 1");
    let r2 = visual.debug_bounds("chat-row-2").expect("row 2");
    // The malicious row reports 40px, so its slot is 40px tall and the
    // neighbours sit where the honest 40px heights dictate.
    assert_eq!(r1.size.height, px(40.), "reported height drives the slot");
    assert_eq!(r0.bottom(), r1.top(), "rows are contiguous");
    assert_eq!(r1.bottom(), r2.top(), "the neighbour slot is not displaced");
}

/// A width change needs no invalidation and no extra frame: the list measures
/// at whatever definite width it is given, so shrinking and restoring leaves no
/// blank band.
#[gpui::test]
async fn width_change_needs_no_invalidation_and_no_extra_frame(cx: &mut TestAppContext) {
    let draw = draw_list(cx, vec![px(40.); 4], 300.);
    draw.config.borrow_mut().keys = keys(4);
    redraw(cx, draw.window);

    let mut visual = VisualTestContext::from_window(draw.window, cx);
    let wide = visual.debug_bounds("chat-row-0").expect("row 0 at 480px");
    drop(visual);

    // Collapse the window to 1px wide, then restore it.
    let visual = VisualTestContext::from_window(draw.window, cx);
    visual.simulate_resize(size(px(1.), px(300.)));
    drop(visual);
    redraw(cx, draw.window);
    let visual = VisualTestContext::from_window(draw.window, cx);
    visual.simulate_resize(size(px(480.), px(300.)));
    drop(visual);
    redraw(cx, draw.window);

    let mut visual = VisualTestContext::from_window(draw.window, cx);
    let restored = visual
        .debug_bounds("chat-row-0")
        .expect("row 0 after resize");
    assert_eq!(
        restored.top(),
        wide.top(),
        "row position is stable across a width round-trip"
    );
}

/// Tail-follow: pinned at the live end, disengages on an upward scroll, and
/// re-arms when the viewport lands back at the bottom.
#[gpui::test]
async fn tail_follow_engages_disengages_and_reengages(cx: &mut TestAppContext) {
    let draw = draw_list(cx, vec![px(40.); 4], 100.);
    draw.config.borrow_mut().keys = keys(4);
    draw.state.follow_tail();
    redraw(cx, draw.window);
    assert_eq!(
        draw.state.is_at_bottom(),
        Some(true),
        "pinned at the bottom"
    );

    // Upward scroll disengages.
    simulate_scroll(cx, draw.window, ScrollDelta::Pixels(point(px(0.), px(20.))));
    assert_eq!(
        draw.state.is_at_bottom(),
        Some(false),
        "scrolled away from the tail"
    );

    // Back to the bottom re-engages.
    simulate_scroll(
        cx,
        draw.window,
        ScrollDelta::Pixels(point(px(0.), px(-120.))),
    );
    assert_eq!(
        draw.state.is_at_bottom(),
        Some(true),
        "re-engaged at the bottom"
    );
}

/// `scroll_to_row` jumps to a message and survives an insertion before it —
/// the key, not the index, is what is remembered.
#[gpui::test]
async fn scroll_to_row_survives_insertion_before_target(cx: &mut TestAppContext) {
    let draw = draw_list(cx, vec![px(20.); 100], 300.);
    let keys = keys(100);
    draw.config.borrow_mut().keys = keys.clone();
    draw.state
        .scroll_to_row(RowKey::from_entity_id(gpui::EntityId::from(50)));
    redraw(cx, draw.window);

    let mut visual = VisualTestContext::from_window(draw.window, cx);
    assert_eq!(
        visual.debug_bounds("chat-row-50").expect("row 50").top(),
        px(0.),
        "scroll_to_row pins the row to the top"
    );
    drop(visual);

    // Insert a row before the target (the message shifts to index 51) and
    // re-key.
    draw.body.borrow_mut().insert(0, px(20.));
    let mut new_keys = Vec::new();
    new_keys.push(RowKey::from_entity_id(gpui::EntityId::from(10_000)));
    new_keys.extend(keys.iter().copied());
    draw.config.borrow_mut().keys = new_keys;
    redraw(cx, draw.window);

    let mut visual = VisualTestContext::from_window(draw.window, cx);
    assert_eq!(
        visual.debug_bounds("chat-row-51").expect("row 51").top(),
        px(0.),
        "the anchor re-resolves to the same message after an insertion"
    );
}

/// Only the rows intersecting the viewport render.
#[gpui::test]
async fn offscreen_rows_are_not_rendered(cx: &mut TestAppContext) {
    let draw = draw_list(cx, vec![px(20.); 10_000], 300.);
    draw.config.borrow_mut().keys = keys(10_000);
    // Reset the counter after the initial draw (which rendered nothing yet),
    // then measure how many rows one frame actually renders.
    *draw.config.borrow().renders.borrow_mut() = 0;
    redraw(cx, draw.window);

    let rendered = *draw.config.borrow().renders.borrow();
    assert!(rendered <= 40, "only visible rows render, got {rendered}");
}

/// Visible rows that grow without any invalidation signal are re-measured on
/// the next frame — the list caches no heights, so nothing needs to be told.
#[gpui::test]
async fn visible_row_growth_reflows_without_invalidation(cx: &mut TestAppContext) {
    // Content (160px) exceeds the 150px viewport, so bottom alignment does not
    // absorb an anchored row.
    let draw = draw_list(cx, vec![px(40.); 4], 150.);
    draw.config.borrow_mut().keys = keys(4);
    // Anchor row 0 to the viewport top.
    draw.state
        .scroll_to_row(RowKey::from_entity_id(gpui::EntityId::from(0)));
    redraw(cx, draw.window);

    let mut visual = VisualTestContext::from_window(draw.window, cx);
    assert_eq!(
        visual.debug_bounds("chat-row-0").expect("row 0").top(),
        px(0.),
        "row 0 is anchored to the viewport top"
    );
    drop(visual);

    // Grow row 0 to 80px: row 1 stays on-screen at 80px.
    draw.body.borrow_mut()[0] = px(80.);
    redraw(cx, draw.window);

    let mut visual = VisualTestContext::from_window(draw.window, cx);
    let after = visual.debug_bounds("chat-row-1").expect("row 1");
    assert_eq!(
        after.top(),
        px(80.),
        "the grown row re-flowed its neighbour without any remeasure call"
    );
}
