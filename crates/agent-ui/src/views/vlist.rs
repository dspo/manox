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
//! - Every row in the draw range is re-measured every frame, not only rows an
//!   explicit `remeasure` flagged. A row's element re-renders each frame, so a
//!   height that changed without a remeasure signal (async image load, font
//!   swap, lazy markdown reflow) would otherwise paint at its fresh height
//!   while the list positions it by the stale cached height — overlapping the
//!   next row or leaving a gap. `remeasure(_items)` only discards an
//!   off-screen row's cached height early.
//! - A width change resets every cached height to the estimate. Each row was
//!   measured against the prior list width, so a resize leaves every cached
//!   height stale; the reset clears off-screen rows so scroll geometry
//!   self-corrects on resize (visible rows re-measure at the new width this
//!   frame regardless).
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
    ScrollDelta, ScrollWheelEvent, Style, StyleRefinement, Styled, Window, point, prelude::*, px,
    relative, size,
};

/// Constant height estimate for items that were never measured. The estimate
/// errs small on purpose: per-row error is bounded by this constant and
/// self-corrects the frame a row enters the overdraw, while a content-derived
/// guess can be unbounded — the blank-region failure this component exists to
/// prevent.
pub const ESTIMATED_ROW_H: f32 = 96.0;

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
}

struct StateInner {
    rows: Vec<Row>,
    overdraw: Pixels,
    follow_mode: FollowMode,
    following: bool,
    anchor: ListOffset,
    pending_jump: Option<ListOffset>,
    /// The list width every cached `Row.height` was measured against. A width
    /// change makes every cached height stale, so a differing width resets each
    /// row to the estimate (visible rows re-measure at the new width this frame
    /// regardless; this only clears stale heights off the draw range so scroll
    /// geometry self-corrects on resize instead of carrying old-width heights).
    last_width: Option<Pixels>,
    viewport_h: Pixels,
    total_h: Pixels,
    scroll_top_px: Pixels,
    scroll_max: Pixels,
}

impl StateInner {
    fn estimated_rows(count: usize) -> Vec<Row> {
        vec![
            Row {
                height: px(ESTIMATED_ROW_H)
            };
            count
        ]
    }

    /// Sum of per-row heights with a leading zero (so `prefix[ix]` is the
    /// top of row `ix` and `prefix[count]` is the content height).
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
            viewport_h: px(0.),
            total_h: px(0.),
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
    /// and any pending jump past the edited range. New rows enter as estimated
    /// (unmeasured) and self-correct the frame they enter the draw range.
    pub fn splice(&self, old_range: Range<usize>, count: usize) {
        let mut s = self.0.borrow_mut();
        let removed = old_range.end.saturating_sub(old_range.start);
        let delta = count as isize - removed as isize;
        let range = old_range.start.min(s.rows.len())..old_range.end.min(s.rows.len());
        s.rows.splice(range, StateInner::estimated_rows(count));
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

    /// Discard every row's cached height, resetting it to the estimate. Visible
    /// rows re-measure at the definite width on the very next frame regardless,
    /// so this only matters for off-screen rows: a stale height (the item was
    /// mutated in place — plan card demoted, steer rolled back) is dropped now
    /// rather than carried until the row scrolls back into the draw range.
    pub fn remeasure(&self) {
        for row in self.0.borrow_mut().rows.iter_mut() {
            row.height = px(ESTIMATED_ROW_H);
        }
    }

    /// Discard the cached height of the rows in `range`. Same contract as
    /// [`Self::remeasure`], scoped to the mutated items.
    pub fn remeasure_items(&self, range: Range<usize>) {
        let mut s = self.0.borrow_mut();
        for row in s.rows.iter_mut().take(range.end).skip(range.start) {
            row.height = px(ESTIMATED_ROW_H);
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

    /// Last prepaint's scroll geometry `(scroll_top, scroll_max, total_h)`.
    /// Snapshot for tests/diagnostics; all zero before the first layout.
    pub fn scroll_geometry(&self) -> (Pixels, Pixels, Pixels) {
        let s = self.0.borrow();
        (s.scroll_top_px, s.scroll_max, s.total_h)
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
                scroll_max: s.scroll_max,
            };
        }

        // A width change invalidates every cached height: each row was measured
        // against the prior width, so its height no longer holds at the new one.
        // Visible rows re-measure at the new width below this frame regardless;
        // this drops the stale heights of off-screen rows so the scroll geometry
        // (scroll_max, total_h) self-corrects on resize instead of carrying
        // old-width heights until each row scrolls back into the draw range.
        // Mirrors gpui::list's width-change invalidation.
        if s.last_width != Some(width) {
            for row in s.rows.iter_mut() {
                row.height = px(ESTIMATED_ROW_H);
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

        let mut prefix = s.prefix_heights();
        let mut scroll_top = resolve_anchor_scroll(&prefix, &mut s.anchor, count);
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

        // Re-measure every row in the draw range at the definite list width,
        // keeping each laid-out element for the paint pass below. Every row
        // is re-measured every frame (not only "dirty" ones): the workspace
        // re-renders each row's element every frame, so a height that changed
        // without an explicit remeasure signal (async image load, font swap,
        // lazy markdown reflow) would otherwise paint at its fresh height
        // while the list positions it by the stale cached height — the row
        // then overlaps its neighbor or leaves a gap. Out-of-range rows keep
        // their last/estimated height for scroll math and self-correct the
        // frame they enter the draw range.
        let measure_space = size(AvailableSpace::Definite(width), AvailableSpace::MinContent);
        let mut elements: Vec<AnyElement> = Vec::with_capacity(end - start);
        let mut height_changed = false;
        for ix in start..end {
            let mut element = (self.render_item)(ix, window, cx);
            let measured = element.layout_as_root(measure_space, window, cx);
            let row = &mut s.rows[ix];
            if row.height != measured.height {
                row.height = measured.height;
                height_changed = true;
            }
            elements.push(element);
        }

        // Re-derive positions from the updated heights. The anchor is
        // re-applied logically — same item, same (clamped) intra-item offset
        // under the new heights — so re-measurement above the anchor never
        // shifts the visible content.
        if height_changed {
            let mut acc = px(0.);
            for (row, slot) in s.rows.iter().zip(prefix.iter_mut().skip(1)) {
                acc += row.height;
                *slot = acc;
            }
            scroll_max = (prefix[count] - viewport_h).max(px(0.));
            if s.following {
                scroll_top = scroll_max;
            } else {
                scroll_top = resolve_anchor_scroll(&prefix, &mut s.anchor, count);
            }
            scroll_top = scroll_top.max(px(0.)).min(scroll_max);
        }
        s.anchor = logical_offset(&prefix, scroll_top, count);

        // Bottom alignment: short conversations sit at the viewport bottom.
        let content_top = (viewport_h - prefix[count]).max(px(0.));

        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            for (i, element) in elements.iter_mut().enumerate() {
                let ix = start + i;
                let origin = bounds.origin + point(px(0.), content_top + prefix[ix] - scroll_top);
                element.prepaint_at(origin, window, cx);
            }
        });

        s.total_h = prefix[count];
        s.scroll_max = scroll_max;
        s.scroll_top_px = scroll_top;

        PrepaintState {
            elements,
            hitbox,
            scroll_max,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            for element in prepaint.elements.iter_mut() {
                element.paint(window, cx);
            }
        });

        let state = self.state.clone();
        let current_view = window.current_view();
        let hitbox_id = prepaint.hitbox.id;
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
            let new_top = (s.scroll_top_px - delta.y)
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

/// Convert a logical anchor to a pixel scroll top under the given prefix
/// heights, normalizing the anchor in place: the index clamps to the item
/// count, and the intra-item offset clamps to the row's current height. The
/// tail sentinel (`item_ix == count`) pins the scroll top past the content
/// end and carries no intra-item offset.
fn resolve_anchor_scroll(prefix: &[Pixels], anchor: &mut ListOffset, count: usize) -> Pixels {
    anchor.item_ix = anchor.item_ix.min(count);
    if anchor.item_ix < count {
        let row_h = prefix[anchor.item_ix + 1] - prefix[anchor.item_ix];
        anchor.offset_in_item = anchor.offset_in_item.min(row_h);
    } else {
        anchor.offset_in_item = px(0.);
    }
    prefix[anchor.item_ix] + anchor.offset_in_item
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

#[cfg(test)]
mod tests {
    use super::*;

    fn prefix_of(row_heights: &[f32]) -> Vec<Pixels> {
        let mut prefix = Vec::with_capacity(row_heights.len() + 1);
        let mut acc = px(0.);
        prefix.push(acc);
        for h in row_heights {
            acc += px(*h);
            prefix.push(acc);
        }
        prefix
    }

    #[test]
    fn prefix_heights_accumulates_rows() {
        let state = VListState::new(0, px(100.));
        state.0.borrow_mut().rows = vec![Row { height: px(10.) }, Row { height: px(20.) }];
        let prefix = state.0.borrow().prefix_heights();
        assert_eq!(prefix, vec![px(0.), px(10.), px(30.)]);
    }

    #[test]
    fn resolve_anchor_tail_sentinel_pins_past_content() {
        // Regression: the tail sentinel (`item_ix == count`) used to feed
        // `row_h(count)`, indexing one past the prefix and panicking.
        let prefix = prefix_of(&[100., 100., 100.]);
        let mut anchor = ListOffset {
            item_ix: 3,
            offset_in_item: px(50.),
        };
        let top = resolve_anchor_scroll(&prefix, &mut anchor, 3);
        assert_eq!(anchor.item_ix, 3);
        assert_eq!(anchor.offset_in_item, px(0.));
        assert_eq!(top, px(300.));
    }

    #[test]
    fn resolve_anchor_clamps_offset_to_row_height() {
        let prefix = prefix_of(&[100., 40., 100.]);
        let mut anchor = ListOffset {
            item_ix: 1,
            offset_in_item: px(80.),
        };
        let top = resolve_anchor_scroll(&prefix, &mut anchor, 3);
        assert_eq!(anchor.offset_in_item, px(40.));
        assert_eq!(top, px(140.));
    }

    #[test]
    fn resolve_anchor_clamps_index_to_count() {
        let prefix = prefix_of(&[100., 100.]);
        let mut anchor = ListOffset {
            item_ix: 9,
            offset_in_item: px(10.),
        };
        let top = resolve_anchor_scroll(&prefix, &mut anchor, 2);
        assert_eq!(anchor.item_ix, 2);
        assert_eq!(top, px(200.));
    }

    #[test]
    fn logical_offset_finds_row_containing_scroll_top() {
        let prefix = prefix_of(&[100., 50., 200.]);
        let off = logical_offset(&prefix, px(120.), 3);
        assert_eq!(off.item_ix, 1);
        assert_eq!(off.offset_in_item, px(20.));
    }

    #[test]
    fn logical_offset_clamps_to_last_row_at_content_end() {
        let prefix = prefix_of(&[100., 50., 200.]);
        let off = logical_offset(&prefix, px(350.), 3);
        assert_eq!(off.item_ix, 2);
        assert_eq!(off.offset_in_item, px(200.));
    }

    #[test]
    fn logical_offset_handles_zero_height_rows() {
        let prefix = prefix_of(&[0., 0., 100.]);
        let off = logical_offset(&prefix, px(0.), 3);
        assert_eq!(off.offset_in_item, px(0.));
    }

    #[test]
    fn anchor_roundtrips_through_logical_offset() {
        // The logical anchor is the source of truth across re-measurement:
        // converting to pixels and back under the same prefix is an identity.
        let prefix = prefix_of(&[100., 50., 200.]);
        let mut anchor = ListOffset {
            item_ix: 2,
            offset_in_item: px(70.),
        };
        let top = resolve_anchor_scroll(&prefix, &mut anchor, 3);
        let back = logical_offset(&prefix, top, 3);
        assert_eq!(back.item_ix, 2);
        assert_eq!(back.offset_in_item, px(70.));
    }

    #[test]
    fn splice_insert_shifts_tail_anchor() {
        let state = VListState::new(5, px(100.));
        state.splice(2..2, 3);
        let s = state.0.borrow();
        assert_eq!(s.rows.len(), 8);
        assert_eq!(s.anchor.item_ix, 8);
        // New rows enter as the constant estimate and self-correct the frame
        // they enter the draw range.
        assert!(s.rows.iter().all(|r| r.height == px(ESTIMATED_ROW_H)));
    }

    #[test]
    fn splice_tail_removal_shrinks_anchor() {
        let state = VListState::new(5, px(100.));
        state.splice(4..5, 0);
        let s = state.0.borrow();
        assert_eq!(s.rows.len(), 4);
        assert_eq!(s.anchor.item_ix, 4);
    }

    #[test]
    fn splice_engulfing_anchor_reanchors_to_range_end() {
        let state = VListState::new(5, px(100.));
        state.0.borrow_mut().anchor = ListOffset {
            item_ix: 2,
            offset_in_item: px(10.),
        };
        state.splice(1..4, 2);
        let s = state.0.borrow();
        assert_eq!(s.anchor.item_ix, 3);
        assert_eq!(s.anchor.offset_in_item, px(0.));
    }

    #[test]
    fn splice_shifts_pending_jump() {
        let state = VListState::new(5, px(100.));
        state.scroll_to(ListOffset {
            item_ix: 4,
            offset_in_item: px(0.),
        });
        state.splice(0..2, 0);
        let s = state.0.borrow();
        assert_eq!(s.pending_jump.unwrap().item_ix, 2);
    }

    #[test]
    fn reset_restores_tail_sentinel_and_clears_state() {
        let state = VListState::new(3, px(100.));
        state.set_follow_mode(FollowMode::Tail);
        state.reset(2);
        let s = state.0.borrow();
        assert_eq!(s.rows.len(), 2);
        assert_eq!(s.anchor.item_ix, 2);
        assert!(!s.following);
        assert!(s.pending_jump.is_none());
        assert!(s.last_width.is_none(), "reset drops the cached width");
    }

    #[test]
    fn scroll_to_end_arms_following_and_pending_jump() {
        let state = VListState::new(3, px(100.));
        state.scroll_to_end();
        let s = state.0.borrow();
        assert!(s.following);
        assert_eq!(s.pending_jump.unwrap().item_ix, 3);
        assert_eq!(s.pending_jump.unwrap().offset_in_item, px(0.));
    }

    #[test]
    fn follow_mode_switch_toggles_following() {
        let state = VListState::new(3, px(100.));
        assert!(!state.is_following_tail());
        state.set_follow_mode(FollowMode::Tail);
        assert!(state.is_following_tail());
        state.set_follow_mode(FollowMode::Normal);
        assert!(!state.is_following_tail());
    }
}
