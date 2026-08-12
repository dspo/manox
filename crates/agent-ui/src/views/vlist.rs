//! Message list backed by gpui-component's `v_virtual_list`.
//!
//! gpui-component owns the virtualization Element, the scroll handle, the
//! visible-range computation, and the content mask. manox owns the two
//! concerns that `v_virtual_list` deliberately leaves to the caller:
//!
//! - Per-item height. `v_virtual_list` lays each visible item out at a
//!   definite, caller-supplied height; it does not measure variable heights
//!   itself. manox measures each visible item at the list's definite content
//!   width (read from the live `content_mask`) with `MinContent` available
//!   height and caches the result, rebuilding the `item_sizes` snapshot each
//!   frame.
//! - Tail-follow arbitration. `FollowMode::Tail` re-pins to the live tail
//!   while no upward user scroll has disengaged it; an upward scroll clears
//!   following, and landing back at the bottom re-arms it. The wheel itself is
//!   fielded by gpui-component's `overflow_scroll` base; manox observes the
//!   resulting offset each render to drive disengage/re-engage.

use std::{cell::RefCell, ops::Range, rc::Rc};

use gpui::{AnyElement, AvailableSpace, Context, Entity, Pixels, Render, Size, Window, px, size};
use gpui_component::{VirtualList, VirtualListScrollHandle, v_virtual_list};

/// Constant height estimate for items that were never measured. Errs small on
/// purpose: per-row error is bounded by this constant and self-corrects the
/// frame a row enters the visible range, while a content-derived guess can be
/// unbounded.
const ESTIMATED_ROW_H: f32 = 96.0;

/// Tail-follow arbitration: `Tail` re-pins to the live end (disengaged by
/// upward user scroll, re-armed when a scroll lands back at the bottom);
/// `Normal` disables it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FollowMode {
    Normal,
    Tail,
}

#[derive(Clone)]
pub struct VListState(Rc<RefCell<StateInner>>);

struct StateInner {
    item_heights: Vec<Pixels>,
    follow_mode: FollowMode,
    following: bool,
    scroll_handle: VirtualListScrollHandle,
    viewport_h: Pixels,
}

impl VListState {
    pub fn new(count: usize) -> Self {
        Self(Rc::new(RefCell::new(StateInner {
            item_heights: vec![px(ESTIMATED_ROW_H); count],
            follow_mode: FollowMode::Normal,
            following: false,
            scroll_handle: VirtualListScrollHandle::new(),
            viewport_h: px(0.),
        })))
    }

    /// Drop all cached heights and re-arm for a fresh conversation.
    pub fn reset(&self, count: usize) {
        let mut s = self.0.borrow_mut();
        s.item_heights = vec![px(ESTIMATED_ROW_H); count];
        s.following = false;
    }

    /// Reconcile the item count (append or tail-removal). New rows enter as
    /// the constant estimate and self-correct the frame they enter the visible
    /// range.
    pub fn splice(&self, old_range: Range<usize>, count: usize) {
        let mut s = self.0.borrow_mut();
        let start = old_range.start.min(s.item_heights.len());
        let end = old_range.end.min(s.item_heights.len());
        let new = vec![px(ESTIMATED_ROW_H); count];
        s.item_heights.splice(start..end, new);
    }

    /// Discard every cached height, resetting to the estimate. Visible rows
    /// re-measure on the next frame regardless; this only matters for off-screen
    /// rows whose cached height would otherwise stay stale until scrolled back.
    pub fn remeasure(&self) {
        let mut s = self.0.borrow_mut();
        for h in s.item_heights.iter_mut() {
            *h = px(ESTIMATED_ROW_H);
        }
    }

    /// Discard the cached height of the rows in `range`.
    pub fn remeasure_items(&self, range: Range<usize>) {
        let mut s = self.0.borrow_mut();
        for h in s.item_heights.iter_mut().take(range.end).skip(range.start) {
            *h = px(ESTIMATED_ROW_H);
        }
    }

    /// Pin to the live tail and re-arm following.
    pub fn scroll_to_end(&self) {
        let mut s = self.0.borrow_mut();
        s.following = true;
        s.scroll_handle.scroll_to_bottom();
    }

    /// Bring the given item to the viewport top.
    pub fn scroll_to(&self, item_ix: usize) {
        let s = self.0.borrow();
        s.scroll_handle
            .scroll_to_item(item_ix, gpui::ScrollStrategy::Top);
    }

    pub fn set_follow_mode(&self, mode: FollowMode) {
        let mut s = self.0.borrow_mut();
        s.follow_mode = mode;
        let now = mode == FollowMode::Tail;
        if now && !s.following {
            s.following = true;
            s.scroll_handle.scroll_to_bottom();
        } else {
            s.following = now;
        }
    }

    pub fn is_following_tail(&self) -> bool {
        self.0.borrow().following
    }

    /// `(scroll_top, scroll_max, total_h)` snapshot for diagnostics/tests.
    /// `gpui::ScrollHandle::offset()` is non-positive (0 = top, `-max` = bottom).
    pub fn scroll_geometry(&self) -> (Pixels, Pixels, Pixels) {
        let s = self.0.borrow();
        let total: Pixels = s.item_heights.iter().copied().sum();
        let max = (total - s.viewport_h).max(px(0.));
        let top = (-s.scroll_handle.base_handle().offset().y)
            .min(max)
            .max(px(0.));
        (top, max, total)
    }

    /// Sum of cached item heights — the content's natural extent.
    pub fn total_height(&self) -> Pixels {
        self.0.borrow().item_heights.iter().copied().sum()
    }

    /// Last observed viewport height (updated each frame from the live content
    /// mask). One-frame stale when read outside the render closure, which is
    /// exactly when the bottom-align spacer reads it.
    pub fn viewport_h(&self) -> Pixels {
        self.0.borrow().viewport_h
    }

    /// Snapshot of per-item sizes for `v_virtual_list`. Only the height field
    /// is consulted in the vertical axis; width is left zero.
    pub fn item_sizes(&self) -> Rc<Vec<Size<Pixels>>> {
        let s = self.0.borrow();
        Rc::new(
            s.item_heights
                .iter()
                .map(|&h| Size::new(px(0.), h))
                .collect(),
        )
    }

    /// Clone of the underlying scroll handle, for `track_scroll` wiring.
    pub fn scroll_handle(&self) -> VirtualListScrollHandle {
        self.0.borrow().scroll_handle.clone()
    }

    /// Record a measured height for item `ix` (called from the render closure).
    pub fn set_height(&self, ix: usize, h: Pixels) {
        let mut s = self.0.borrow_mut();
        if ix < s.item_heights.len() && s.item_heights[ix] != h {
            s.item_heights[ix] = h;
        }
    }

    /// Observe the current scroll offset and viewport, driving tail-follow
    /// disengage (upward scroll) / re-engage (back at bottom). Called from the
    /// render closure each frame with the live content-mask height. Only
    /// arbitrates under `Tail` mode — `Normal` mode leaves `following` untouched
    /// (its value is set explicitly by `set_follow_mode` on the mode switch).
    pub fn arbitrate_tail_follow(&self, viewport_h: Pixels) {
        let mut s = self.0.borrow_mut();
        s.viewport_h = viewport_h;
        if viewport_h <= px(0.) || s.item_heights.is_empty() || s.follow_mode != FollowMode::Tail {
            return;
        }
        let total: Pixels = s.item_heights.iter().copied().sum();
        let max = (total - viewport_h).max(px(0.));
        let offset_y = -s.scroll_handle.base_handle().offset().y;
        // At (or past) the bottom: re-arm following so the live tail stays
        // pinned as new content arrives; any non-bottom scroll disengages it.
        s.following = offset_y >= max - px(1.);
    }
}

type RenderItemFn<V> = dyn Fn(&mut V, usize, &mut Window, &mut Context<V>) -> AnyElement + 'static;

/// Build a gpui-component `VirtualList` for the conversation. manox measures
/// each visible item inside the render closure (at the live content-mask width)
/// and feeds the cached heights back as `item_sizes` on the next frame.
pub fn vlist<V: Render + 'static>(
    view: Entity<V>,
    state: VListState,
    render_item: impl Fn(&mut V, usize, &mut Window, &mut Context<V>) -> AnyElement + 'static,
) -> VirtualList {
    let item_sizes = state.item_sizes();
    let scroll_handle = state.scroll_handle();
    let measure_state = state.clone();
    let render_item: Box<RenderItemFn<V>> = Box::new(render_item);
    v_virtual_list(
        view,
        "msg-list",
        item_sizes,
        move |this, range, window, cx| {
            let width = window.content_mask().bounds.size.width;
            let viewport_h = window.content_mask().bounds.size.height;
            measure_state.arbitrate_tail_follow(viewport_h);
            let can_measure = width > px(0.);
            let measure_space = size(AvailableSpace::Definite(width), AvailableSpace::MinContent);
            range
                .map(|ix| {
                    let mut el = render_item(this, ix, window, cx);
                    if can_measure {
                        let measured = el.layout_as_root(measure_space, window, cx);
                        measure_state.set_height(ix, measured.height);
                    }
                    el
                })
                .collect::<Vec<AnyElement>>()
        },
    )
    .track_scroll(&scroll_handle)
}
