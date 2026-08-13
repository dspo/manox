//! Message-list adapter for gpui-component's `v_virtual_list`.
//!
//! gpui-component owns virtualization, clipping, and wheel scrolling. The
//! adapter measures every rendered row at the definite viewport width and
//! feeds those heights back on the following frame. Unmeasured rows use a
//! deliberately small fixed estimate so an unknown row cannot create a huge
//! blank region.

use std::{cell::RefCell, ops::Range, rc::Rc};

use gpui::{AnyElement, AvailableSpace, Context, Entity, Pixels, Render, Size, Window, px, size};
use gpui_component::{VirtualList, VirtualListScrollHandle, v_virtual_list};

const ESTIMATED_ROW_H: f32 = 96.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FollowMode {
    Normal,
    Tail,
}

#[derive(Clone, Copy, Debug)]
pub struct ListOffset {
    pub item_ix: usize,
    pub offset_in_item: Pixels,
}

#[derive(Clone)]
pub struct VListState(Rc<RefCell<StateInner>>);

struct StateInner {
    item_heights: Vec<Pixels>,
    follow_mode: FollowMode,
    following: bool,
    scroll_handle: VirtualListScrollHandle,
    viewport_h: Pixels,
    last_viewport_h: Pixels,
    last_total_h: Pixels,
    last_offset_y: Pixels,
}

impl VListState {
    pub fn new(count: usize) -> Self {
        Self(Rc::new(RefCell::new(StateInner {
            item_heights: vec![px(ESTIMATED_ROW_H); count],
            follow_mode: FollowMode::Normal,
            following: false,
            scroll_handle: VirtualListScrollHandle::new(),
            viewport_h: px(0.),
            last_viewport_h: px(0.),
            last_total_h: px(ESTIMATED_ROW_H * count as f32),
            last_offset_y: px(0.),
        })))
    }

    pub fn reset(&self, count: usize) {
        let mut state = self.0.borrow_mut();
        state.item_heights = vec![px(ESTIMATED_ROW_H); count];
        state.following = false;
        state.last_viewport_h = px(0.);
        state.last_total_h = px(ESTIMATED_ROW_H * count as f32);
        state.last_offset_y = px(0.);
    }

    pub fn splice(&self, old_range: Range<usize>, count: usize) {
        let mut state = self.0.borrow_mut();
        let start = old_range.start.min(state.item_heights.len());
        let end = old_range.end.min(state.item_heights.len());
        state
            .item_heights
            .splice(start..end, vec![px(ESTIMATED_ROW_H); count]);
    }

    pub fn remeasure(&self) {
        for height in &mut self.0.borrow_mut().item_heights {
            *height = px(ESTIMATED_ROW_H);
        }
    }

    pub fn remeasure_items(&self, range: Range<usize>) {
        let mut state = self.0.borrow_mut();
        for height in state
            .item_heights
            .iter_mut()
            .take(range.end)
            .skip(range.start)
        {
            *height = px(ESTIMATED_ROW_H);
        }
    }

    pub fn scroll_to_end(&self) {
        let mut state = self.0.borrow_mut();
        state.following = true;
        state.scroll_handle.scroll_to_bottom();
    }

    pub fn scroll_to(&self, offset: ListOffset) {
        let state = self.0.borrow();
        let _ = offset.offset_in_item;
        state
            .scroll_handle
            .scroll_to_item(offset.item_ix, gpui::ScrollStrategy::Top);
    }

    pub fn set_follow_mode(&self, mode: FollowMode) {
        let mut state = self.0.borrow_mut();
        state.follow_mode = mode;
        state.following = mode == FollowMode::Tail;
        if state.following {
            state.scroll_handle.scroll_to_bottom();
        }
    }

    pub fn is_following_tail(&self) -> bool {
        self.0.borrow().following
    }

    pub fn scroll_geometry(&self) -> (Pixels, Pixels, Pixels) {
        let state = self.0.borrow();
        let total: Pixels = state.item_heights.iter().copied().sum();
        let max = (total - state.viewport_h).max(px(0.));
        let top = (-state.scroll_handle.base_handle().offset().y)
            .min(max)
            .max(px(0.));
        (top, max, total)
    }

    pub fn total_height(&self) -> Pixels {
        self.0.borrow().item_heights.iter().copied().sum()
    }

    pub fn viewport_h(&self) -> Pixels {
        self.0.borrow().viewport_h
    }

    fn item_sizes(&self) -> Rc<Vec<Size<Pixels>>> {
        Rc::new(
            self.0
                .borrow()
                .item_heights
                .iter()
                .map(|height| Size::new(px(0.), *height))
                .collect(),
        )
    }

    fn scroll_handle(&self) -> VirtualListScrollHandle {
        self.0.borrow().scroll_handle.clone()
    }

    fn set_height(&self, ix: usize, height: Pixels) -> bool {
        let mut state = self.0.borrow_mut();
        let Some(slot) = state.item_heights.get_mut(ix) else {
            return false;
        };
        if *slot == height {
            return false;
        }
        *slot = height;
        true
    }

    /// Reconcile tail-follow before constructing gpui-component's element.
    /// This must run outside VirtualList's render-range callback: that callback
    /// is invoked while gpui-component mutably leases its scroll state, and
    /// calling `scroll_to_bottom` there is a nested RefCell borrow.
    fn prepare_frame(&self) {
        let mut state = self.0.borrow_mut();
        if state.viewport_h <= px(0.) || state.item_heights.is_empty() {
            return;
        }

        let total: Pixels = state.item_heights.iter().copied().sum();
        let max = (total - state.viewport_h).max(px(0.));
        let offset_y = (-state.scroll_handle.base_handle().offset().y)
            .max(px(0.))
            .min(max);

        if state.follow_mode == FollowMode::Tail {
            let content_unchanged = total == state.last_total_h;
            let viewport_unchanged = state.viewport_h == state.last_viewport_h;
            let user_moved_up =
                content_unchanged && viewport_unchanged && offset_y < state.last_offset_y - px(1.);
            if state.following && user_moved_up {
                state.following = false;
            } else if !state.following && offset_y >= max - px(1.) {
                state.following = true;
            }
        }

        state.last_total_h = total;
        state.last_viewport_h = state.viewport_h;
        state.last_offset_y = offset_y;
        let follow = state.following;
        let handle = state.scroll_handle.clone();
        drop(state);
        if follow {
            handle.scroll_to_bottom();
        }
    }

    fn observe_viewport(&self, viewport_h: Pixels) -> bool {
        let mut state = self.0.borrow_mut();
        if state.viewport_h == viewport_h {
            return false;
        }
        state.viewport_h = viewport_h;
        true
    }
}

type RenderItemFn<V> = dyn Fn(&mut V, usize, &mut Window, &mut Context<V>) -> AnyElement + 'static;

pub fn vlist<V: Render + 'static>(
    view: Entity<V>,
    state: VListState,
    render_item: impl Fn(&mut V, usize, &mut Window, &mut Context<V>) -> AnyElement + 'static,
) -> VirtualList {
    state.prepare_frame();
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
            let viewport_changed = measure_state.observe_viewport(viewport_h);
            let can_measure = width > px(0.);
            let measure_space = size(AvailableSpace::Definite(width), AvailableSpace::MinContent);
            let mut changed = false;
            let rows = range
                .map(|ix| {
                    let mut element = render_item(this, ix, window, cx);
                    if can_measure {
                        let measured = element.layout_as_root(measure_space, window, cx);
                        changed |= measure_state.set_height(ix, measured.height);
                    }
                    element
                })
                .collect::<Vec<_>>();
            if changed || viewport_changed {
                // `item_sizes` is an immutable per-frame snapshot. Schedule a
                // frame that rebuilds the VirtualList with the measured sizes.
                // A first non-zero viewport also needs a second frame so the
                // scroll handle's item count is initialized before tail-follow
                // resolves its deferred bottom jump.
                cx.notify();
            }
            rows
        },
    )
    .track_scroll(&scroll_handle)
}
