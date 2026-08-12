//! First-party virtualized message list (`vlist`).
//!
//! Replaces `gpui::list` for the conversation view. The gpui list caches
//! per-item heights in a sum tree whose measurement passes can, under edge
//! conditions (zero/min-content width frames, stale caches across splices),
//! store exploded heights that surface as huge blank regions between
//! messages. This component owns its height cache outright and enforces the
//! invariants that prevent that class of bug:
//!
//! - Items are only ever measured at the list's definite pixel width; a frame
//!   with a non-positive width measures nothing and keeps the old cache.
//! - Unmeasured items carry a small constant estimate, never a content-derived
//!   guess, so a wrong estimate errs toward "too small", not "ten screens".
//! - The scroll position is a logical anchor (item index + offset), so height
//!   re-measurement above the anchor never shifts the visible content.
//!
//! Feature surface mirrors what the workspace uses from `ListState`:
//! `reset`, `splice`, `remeasure(_items)`, `scroll_to(_end)`,
//! `set_follow_mode`, `is_following_tail` — plus bottom alignment (chat-log
//! semantics) and overdraw rendering. No scrollbar: the gpui list rendered
//! none for this view either.

use std::{cell::RefCell, ops::Range, rc::Rc};

use gpui::{
    AnyElement, App, AvailableSpace, Bounds, ContentMask, DispatchPhase, Element, ElementId,
    GlobalElementId, Hitbox, HitboxBehavior, InspectorElementId, IntoElement, LayoutId, Pixels,
    Point, ScrollDelta, ScrollWheelEvent, Style, StyleRefinement, Styled, Window, point,
    prelude::*, px, relative, size,
};

/// Constant height estimate for items that were never measured. Deliberately
/// small: an underestimate only tightens the tail briefly (the item is
/// measured the frame it enters the overdraw), while an overestimate is
/// exactly the blank-region failure this component exists to prevent.
const ESTIMATED_ROW_H: f32 = 96.0;

/// Follow-tail arbitration, matching the workspace's previous usage of gpui's
/// `FollowMode`: `Tail` arms auto-pinning (disengaged by upward user scrolls,
/// re-engaged when a scroll lands back at the bottom); `Normal` disables it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FollowMode {
    Normal,
    Tail,
}

/// Logical scroll top: the item at the viewport top and the pixel offset into
/// it. Logical (not pixel) anchoring keeps the visible content stable across
/// height re-measurement anywhere in the list.
#[derive(Clone, Copy, Debug)]
pub struct ListOffset {
    pub item_ix: usize,
    pub offset_in_item: Pixels,
}

#[derive(Clone)]
pub struct VListState(Rc<RefCell<StateInner>>);

#[derive(Clone, Copy)]
struct Row {
    height: Pixels,
    measured: bool,
    dirty: bool,
}

struct StateInner {
    rows: Vec<Row>,
    overdraw: Pixels,
    follow_mode: FollowMode,
    following: bool,
    anchor: ListOffset,
    pending_jump: Option<ListOffset>,
    last_width: Option<Pixels>,
    total_h: Pixels,
    viewport_h: Pixels,
    scroll_top_px: Pixels,
    scroll_max: Pixels,
}

impl StateInner {
    fn estimated_rows(count: usize) -> Vec<Row> {
        vec![
            Row {
                height: px(ESTIMATED_ROW_H),
                measured: false,
                dirty: false,
            };
            count
        ]
    }

    fn prefix_heights(&self) -> Vec<Pixels> {
        let mut prefix = Vec::with_capacity(self.rows.len() + 1);
        let mut acc = px(0.);
        prefix.push(acc);
        for row in &self.rows {
            acc += row.height;
            prefix.push(acc);
        }
        prefix
    }
}

impl VListState {
    pub fn new(count: usize, overdraw: Pixels) -> Self {
        Self(Rc::new(RefCell::new(StateInner {
            rows: StateInner::estimated_rows(count),
            overdraw,
            follow_mode: FollowMode::Normal,
            following: false,
            anchor: ListOffset {
                item_ix: count,
                offset_in_item: px(0.),
            },
            pending_jump: None,
            last_width: None,
            total_h: px(0.),
            viewport_h: px(0.),
            scroll_top_px: px(0.),
            scroll_max: px(0.),
        })))
    }

    /// Drop all cached heights and re-arm for a fresh conversation.
    pub fn reset(&self, count: usize) {
        let mut s = self.0.borrow_mut();
        s.rows = StateInner::estimated_rows(count);
        s.anchor = ListOffset {
            item_ix: count,
            offset_in_item: px(0.),
        };
        s.pending_jump = None;
        s.following = false;
        s.last_width = None;
    }

    /// Reconcile the item count (append or tail-removal), shifting the anchor
    /// and any pending jump past the edited range.
    pub fn splice(&self, old_range: Range<usize>, count: usize) {
        let mut s = self.0.borrow_mut();
        let removed = old_range.end.saturating_sub(old_range.start);
        let delta = count as isize - removed as isize;
        let range = old_range.start.min(s.rows.len())..old_range.end.min(s.rows.len());
        s.rows.splice(
            range,
            (0..count).map(|_| Row {
                height: px(ESTIMATED_ROW_H),
                measured: false,
                dirty: false,
            }),
        );
        let shift = |off: &mut ListOffset| {
            if off.item_ix >= old_range.end {
                off.item_ix = (off.item_ix as isize + delta).max(0) as usize;
            } else if off.item_ix > old_range.start {
                off.item_ix = old_range.start + count;
                off.offset_in_item = px(0.);
            }
        };
        shift(&mut s.anchor);
        if let Some(jump) = s.pending_jump.as_mut() {
            shift(jump);
        }
    }

    /// Mark every item for re-measurement (width-independent content changes).
    pub fn remeasure(&self) {
        for row in self.0.borrow_mut().rows.iter_mut() {
            row.dirty = true;
        }
    }

    /// Mark a range for re-measurement.
    pub fn remeasure_items(&self, range: Range<usize>) {
        let mut s = self.0.borrow_mut();
        for row in s.rows.iter_mut().take(range.end).skip(range.start) {
            row.dirty = true;
        }
    }

    /// Pin the viewport to the live tail and re-arm following.
    pub fn scroll_to_end(&self) {
        let mut s = self.0.borrow_mut();
        s.pending_jump = Some(ListOffset {
            item_ix: s.rows.len(),
            offset_in_item: px(0.),
        });
        s.following = true;
    }

    /// Place the given item at the viewport top.
    pub fn scroll_to(&self, offset: ListOffset) {
        self.0.borrow_mut().pending_jump = Some(offset);
    }

    pub fn set_follow_mode(&self, mode: FollowMode) {
        let mut s = self.0.borrow_mut();
        s.follow_mode = mode;
        s.following = mode == FollowMode::Tail;
    }

    pub fn is_following_tail(&self) -> bool {
        self.0.borrow().following
    }
}

type RenderItemFn = dyn FnMut(usize, &mut Window, &mut App) -> AnyElement + 'static;

pub fn vlist(
    state: VListState,
    render_item: impl FnMut(usize, &mut Window, &mut App) -> AnyElement + 'static,
) -> VList {
    VList {
        state,
        render_item: Box::new(render_item),
        style: StyleRefinement::default(),
    }
}

pub struct VList {
    state: VListState,
    render_item: Box<RenderItemFn>,
    style: StyleRefinement,
}

impl Styled for VList {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

pub struct PrepaintState {
    elements: Vec<AnyElement>,
    hitbox: Hitbox,
    scroll_top_px: Pixels,
    scroll_max: Pixels,
}

impl Element for VList {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        style.refine(&self.style);
        let layout_id = window.request_layout(style, [], cx);
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> PrepaintState {
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        let width = bounds.size.width;
        let viewport_h = bounds.size.height;
        let mut s = self.state.0.borrow_mut();
        s.viewport_h = viewport_h;

        // A zero-size frame (hidden/collapsed container, first layout pass
        // before the parent resolved) measures nothing: measuring at zero or
        // min-content width is precisely what explodes text heights.
        if width <= px(0.) || viewport_h <= px(0.) || s.rows.is_empty() {
            s.total_h = s.rows.iter().map(|r| r.height).sum();
            s.scroll_max = (s.total_h - viewport_h).max(px(0.));
            return PrepaintState {
                elements: Vec::new(),
                hitbox,
                scroll_top_px: s.scroll_top_px,
                scroll_max: s.scroll_max,
            };
        }

        // A width change invalidates every cached height.
        if s.last_width != Some(width) {
            for row in s.rows.iter_mut() {
                row.dirty = true;
            }
            s.last_width = Some(width);
        }

        let count = s.rows.len();

        // Resolve the logical scroll top for this frame.
        if let Some(jump) = s.pending_jump.take() {
            s.anchor = jump;
        } else if s.following {
            s.anchor = ListOffset {
                item_ix: count,
                offset_in_item: px(0.),
            };
        }
        s.anchor.item_ix = s.anchor.item_ix.min(count);

        let mut prefix = s.prefix_heights();
        let row_h = |prefix: &[Pixels], ix: usize| prefix[ix + 1] - prefix[ix];
        s.anchor.offset_in_item = s
            .anchor
            .offset_in_item
            .min(row_h(&prefix, s.anchor.item_ix));
        let mut scroll_top = prefix[s.anchor.item_ix] + s.anchor.offset_in_item;
        let mut scroll_max = (prefix[count] - viewport_h).max(px(0.));
        if s.following {
            scroll_top = scroll_max;
        }
        scroll_top = scroll_top.max(px(0.)).min(scroll_max);

        // Draw range: visible window plus overdraw on both sides.
        let first_visible = partition_point(&prefix, |h| h <= scroll_top)
            .saturating_sub(1)
            .min(count - 1);
        let mut start = first_visible;
        while start > 0 && prefix[start + 1] > scroll_top - s.overdraw {
            start -= 1;
        }
        let mut end = first_visible;
        while end < count && prefix[end] < scroll_top + viewport_h + s.overdraw {
            end += 1;
        }

        // Measure dirty/unmeasured rows in the draw range at the definite
        // list width, keeping the laid-out element for prepaint below.
        let measure_space = size(AvailableSpace::Definite(width), AvailableSpace::MinContent);
        let mut laid_out: Vec<Option<(Point<Pixels>, AnyElement)>> =
            Vec::with_capacity(end - start);
        let mut measured_any = false;
        for ix in start..end {
            let needs = {
                let row = &s.rows[ix];
                row.dirty || !row.measured
            };
            if needs {
                let mut element = (self.render_item)(ix, window, cx);
                let measured = element.layout_as_root(measure_space, window, cx);
                let row = &mut s.rows[ix];
                row.height = measured.height;
                row.measured = true;
                row.dirty = false;
                measured_any = true;
                laid_out.push(Some((point(px(0.), px(0.)), element)));
            } else {
                laid_out.push(None);
            }
        }

        // Re-derive positions from the updated heights; the logical anchor
        // keeps the visible content stable across re-measurement.
        if measured_any {
            let mut acc = px(0.);
            for (row, slot) in s.rows.iter().zip(prefix.iter_mut().skip(1)) {
                acc += row.height;
                *slot = acc;
            }
            scroll_max = (prefix[count] - viewport_h).max(px(0.));
            if s.following {
                scroll_top = scroll_max;
            }
            scroll_top = scroll_top.max(px(0.)).min(scroll_max);
        }
        s.anchor = logical_offset(&prefix, scroll_top, count);

        // Bottom alignment: short conversations sit at the viewport bottom.
        let content_top = (viewport_h - prefix[count]).max(px(0.));

        let mut elements: Vec<AnyElement> = Vec::with_capacity(end - start);
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            for (ix, slot) in (start..end).zip(laid_out) {
                let mut element = match slot {
                    Some((_, element)) => element,
                    None => {
                        let mut element = (self.render_item)(ix, window, cx);
                        let _ = element.layout_as_root(measure_space, window, cx);
                        element
                    }
                };
                let origin = bounds.origin + point(px(0.), content_top + prefix[ix] - scroll_top);
                element.prepaint_at(origin, window, cx);
                elements.push(element);
            }
        });

        s.total_h = prefix[count];
        s.scroll_max = scroll_max;
        s.scroll_top_px = scroll_top;

        PrepaintState {
            elements,
            hitbox,
            scroll_top_px: scroll_top,
            scroll_max,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        for element in prepaint.elements.iter_mut() {
            element.paint(window, cx);
        }

        let state = self.state.clone();
        let current_view = window.current_view();
        let hitbox_id = prepaint.hitbox.id;
        let scroll_top_px = prepaint.scroll_top_px;
        let scroll_max = prepaint.scroll_max;
        let mut accumulated = ScrollDelta::default();
        window.on_mouse_event(move |event: &ScrollWheelEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble || !hitbox_id.should_handle_scroll(window) {
                return;
            }
            accumulated = accumulated.coalesce(event.delta);
            let delta = accumulated.pixel_delta(px(20.));
            if delta.y == px(0.) {
                return;
            }
            let mut s = state.0.borrow_mut();
            let base = if s.scroll_top_px == scroll_top_px {
                scroll_top_px
            } else {
                s.scroll_top_px
            };
            let new_top = (base - delta.y)
                .max(px(0.))
                .min(s.scroll_max.max(scroll_max));
            if delta.y > px(0.) {
                s.following = false;
            }
            if s.follow_mode == FollowMode::Tail && new_top >= s.scroll_max - px(1.) {
                s.following = true;
            }
            s.scroll_top_px = new_top;
            s.anchor = logical_offset(&s.prefix_heights(), new_top, s.rows.len());
            drop(s);
            cx.notify(current_view);
        });
    }
}

impl IntoElement for VList {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

/// Largest index whose prefix height is <= target (binary search).
fn partition_point(v: &[Pixels], mut pred: impl FnMut(Pixels) -> bool) -> usize {
    let mut lo = 0;
    let mut hi = v.len();
    while lo < hi {
        let mid = (lo + hi) / 2;
        if pred(v[mid]) {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

fn logical_offset(prefix: &[Pixels], scroll_top: Pixels, count: usize) -> ListOffset {
    let ix = partition_point(prefix, |h| h <= scroll_top)
        .saturating_sub(1)
        .min(count.saturating_sub(1));
    ListOffset {
        item_ix: ix,
        offset_in_item: (scroll_top - prefix[ix]).max(px(0.)),
    }
}
