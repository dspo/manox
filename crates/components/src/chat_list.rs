//! `ChatList` — an anchor-only virtualized list for chat logs.
//!
//! The list caches nothing: its state is a single anchor — which row straddles
//! the viewport top and by how much — plus a tail-follow flag. Every visible
//! row is measured at a definite width each frame (`request_measured_layout`
//! style constraints), so no cache can go stale and no invalidation surface
//! (`remeasure`/`splice`/width invalidators) exists to mis-fire. Rows are keyed
//! by `RowKey`, so `scroll_to_row` survives row insertions before the target —
//! an index-based scroll cannot.
//!
//! A row that paints taller than it measures is clamped to its own slot by a
//! per-row `ContentMask`, downgrading any residual height inconsistency from
//! "overlap the next row" to "clip itself". This is the third layer of the
//! fix; the first two are the `RichText` measured-leaf contract and the lack
//! of any cached heights.

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;

use gpui::{
    App, AvailableSpace, Bounds, ContentMask, DispatchPhase, Element, ElementId, GlobalElementId,
    HitboxBehavior, HitboxId, InspectorElementId, IntoElement, LayoutId, ListAlignment, Pixels,
    Point, ScrollWheelEvent, Size, Style, StyleRefinement, Styled, Window, point, prelude::*, px,
    size,
};

/// Row renderer: one index into the caller's item list to a measured element.
type RowRenderer = dyn FnMut(usize, &mut Window, &mut App) -> gpui::AnyElement;

/// Newtype over a gpui `EntityId`: stable, unique, `Copy+Eq+Hash`, and every
/// message row already is an `Entity<MessageItem>`, so no new API is needed to
/// derive keys for the conversation.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct RowKey(pub(crate) gpui::EntityId);

impl RowKey {
    pub fn from_entity_id(id: gpui::EntityId) -> Self {
        Self(id)
    }

    pub fn entity_id(&self) -> gpui::EntityId {
        self.0
    }
}

/// Where the viewport sits in the document. Only one anchor exists; nothing
/// else is remembered across frames.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Anchor {
    /// Row `key` has its top edge `offset` px above the viewport top. Frame-end
    /// invariant: the row straddles the viewport top and `0 <= offset < h(key)`.
    Top {
        key: RowKey,
        ix_hint: usize,
        offset: Pixels,
    },
    /// The last row's bottom sits on the viewport bottom. A distinguished
    /// value rather than `Top{last, h}` — the last row's height is unknown
    /// without measuring, and this list refuses to cache heights.
    End,
}

/// A user-initiated scroll request, consumed at the next prepaint.
#[derive(Clone, Copy, Debug)]
pub(crate) enum PendingScroll {
    ToEnd,
    ToRow(RowKey),
}

/// Layout state consumed and produced by `plan_frame`. The shared handle keeps
/// this so scroll requests can be queued from event handlers (non-paint
/// frames) and the anchor survives across frames without element state.
#[derive(Clone, Copy, Debug)]
pub(crate) struct State {
    anchor: Anchor,
    following_tail: bool,
    pending: Option<PendingScroll>,
    pending_delta_y: Pixels,
}

impl Default for State {
    fn default() -> Self {
        Self {
            anchor: Anchor::End,
            following_tail: true,
            pending: None,
            pending_delta_y: px(0.),
        }
    }
}

/// Shared handle to the list's scroll state. Methods mutate the inner cell so
/// they are callable from any frame, not just during element drawing.
#[derive(Clone, Default)]
pub struct ChatListState(Rc<RefCell<Inner>>);

struct Inner {
    state: State,
    align: ListAlignment,
    overdraw: Pixels,
    last: Option<FrameReport>,
    view: Option<gpui::EntityId>,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            state: State::default(),
            align: ListAlignment::Bottom,
            overdraw: px(0.),
            last: None,
            view: None,
        }
    }
}

impl ChatListState {
    /// Short histories sit at the viewport bottom, long ones scroll — chat-log
    /// semantics. Starts tail-following.
    pub fn bottom_aligned() -> Self {
        Self(Rc::new(RefCell::new(Inner::default())))
    }

    /// Rows below the viewport are measured this far ahead so scrolling never
    /// pops an unmeasured row into view.
    pub fn with_overdraw(self, overdraw: Pixels) -> Self {
        self.0.borrow_mut().overdraw = overdraw;
        self
    }

    /// Point the list at the entity whose events drive it; scroll deltas notify
    /// it to request a repaint.
    pub fn set_view(&self, view: gpui::EntityId) {
        self.0.borrow_mut().view = Some(view);
    }

    /// Jump to the live end and re-engage tail-follow.
    pub fn follow_tail(&self) {
        let mut inner = self.0.borrow_mut();
        inner.state.following_tail = true;
        inner.state.pending = Some(PendingScroll::ToEnd);
    }

    /// Jump so `key`'s top is at the viewport top, disengaging tail-follow.
    pub fn scroll_to_row(&self, key: RowKey) {
        let mut inner = self.0.borrow_mut();
        inner.state.following_tail = false;
        inner.state.pending = Some(PendingScroll::ToRow(key));
    }

    /// Jump to the live end. Unlike `follow_tail`, the tail-follow flag is left
    /// untouched — an event-driven re-pin that must not yank a scrolled-away
    /// viewport back to following uses this.
    pub fn scroll_to_end(&self) {
        self.0.borrow_mut().state.pending = Some(PendingScroll::ToEnd);
    }

    /// Whether the viewport currently sits at the bottom. `None` before the
    /// first frame laid anything out.
    pub fn is_at_bottom(&self) -> Option<bool> {
        self.0.borrow().last.as_ref().map(|r| r.at_bottom)
    }

    /// Whether the list is still pinned to the live end.
    pub fn is_following_tail(&self) -> bool {
        self.0.borrow().state.following_tail
    }

    /// The range of row indices rendered last frame.
    pub fn visible_range(&self) -> Option<Range<usize>> {
        self.0.borrow().last.as_ref().map(|r| r.visible.clone())
    }

    /// The row currently straddling the viewport top, if any.
    pub fn anchor(&self) -> Option<RowKey> {
        match self.0.borrow().state.anchor {
            Anchor::Top { key, .. } => Some(key),
            Anchor::End => None,
        }
    }
}

/// Pure description of one frame's placement; stale or missing values cannot
/// affect layout because nothing reads it back.
#[derive(Clone, Debug)]
pub struct FrameReport {
    pub viewport: Bounds<Pixels>,
    /// Range of row indices measured and placed this frame.
    pub placed: Range<usize>,
    /// Range of row indices intersecting the viewport.
    pub visible: Range<usize>,
    pub at_top: bool,
    pub at_bottom: bool,
    /// True when the whole content fits below the viewport height.
    pub underflow: bool,
    pub count: usize,
}

/// Row height source. `Element::prepaint` wraps `layout_as_root` in this so
/// the whole frame plan is testable without a window.
pub trait HeightOracle {
    fn height(&mut self, ix: usize) -> Pixels;
}

/// One row's index and its window-coordinate slot.
#[derive(Debug)]
pub struct PlacedRow {
    pub ix: usize,
    pub slot: Bounds<Pixels>,
}

/// A frame's placement decision.
pub struct FramePlan {
    pub rows: Vec<PlacedRow>,
    pub report: FrameReport,
    /// Scroll delta not consumed this frame (budget exhaustion).
    pub remaining_delta_y: Pixels,
    pub budget_exhausted: bool,
}

/// Hard cap on row measurements per frame. Exhausting it stops the fill and
/// re-notifies for another frame, so a violent scroll settles as gradual
/// progress instead of a dead lock.
pub const MAX_MEASURES_PER_FRAME: usize = 512;

/// Compute which rows to place for this frame. Pure geometry: `heights` is the
/// only side-effecting input, so every boundary branch (bottom alignment,
/// clamping, tail re-engagement, key resolution, budget truncation) is covered
/// by plain `#[test]`s with a fixed-height oracle.
///
/// Each row's `height()` is consulted exactly once per frame — the anchored
/// seed row is measured here, and the fills start at its immediate neighbour —
/// so the measurement budget doubles as a per-row count.
pub(crate) fn plan_frame(
    state: &mut State,
    align: ListAlignment,
    viewport: Bounds<Pixels>,
    overdraw: Pixels,
    keys: &[RowKey],
    heights: &mut dyn HeightOracle,
    budget: usize,
) -> FramePlan {
    plan_frame_inner(state, align, viewport, overdraw, keys, heights, budget, 0)
}

// `plan_frame` is a pure geometry function; its many parameters are the price
// of staying side-effect-free and testable without a window.
#[allow(clippy::too_many_arguments)]
fn plan_frame_inner(
    state: &mut State,
    align: ListAlignment,
    viewport: Bounds<Pixels>,
    overdraw: Pixels,
    keys: &[RowKey],
    heights: &mut dyn HeightOracle,
    budget: usize,
    depth: usize,
) -> FramePlan {
    let count = keys.len();
    let width = viewport.size.width;
    let height = viewport.size.height;

    // Degenerate frames leave the anchor untouched; pending scroll requests
    // survive to fire once a real viewport exists.
    if width <= px(0.) || height <= px(0.) || count == 0 {
        return FramePlan {
            rows: Vec::new(),
            report: FrameReport {
                viewport,
                placed: 0..0,
                visible: 0..0,
                at_top: true,
                at_bottom: true,
                underflow: true,
                count,
            },
            remaining_delta_y: px(0.),
            budget_exhausted: false,
        };
    }

    // A. Consume pending requests, then let tail-follow override them.
    if let Some(pending) = state.pending.take() {
        match pending {
            PendingScroll::ToEnd => state.anchor = Anchor::End,
            PendingScroll::ToRow(key) => {
                state.anchor = Anchor::Top {
                    key,
                    ix_hint: 0,
                    offset: px(0.),
                }
            }
        }
    }
    if state.following_tail {
        state.anchor = Anchor::End;
    }

    let mut used = 0usize;
    let mut rows: Vec<PlacedRow> = Vec::new();
    let mut up_tmp: Vec<PlacedRow> = Vec::new();
    let mut placed_lo = usize::MAX;
    let mut placed_hi = 0usize;
    let mut exhausted = false;

    // B. Seed: the anchored row pins the frame. Measured once and placed here;
    //    the fills below start at its neighbours.
    let (seed_ix, seed_h, seed_top_y) = match state.anchor {
        Anchor::Top {
            key,
            ix_hint,
            offset,
        } => {
            let ix = resolve_key(keys, key, ix_hint);
            if used >= budget {
                return truncated_frame(viewport, count, rows, up_tmp);
            }
            let h = heights.height(ix);
            used += 1;
            // The anchored row straddles the viewport top, so clamp the offset
            // into `[0, h)` even if a prior frame reported an out-of-range one.
            let offset = offset.max(px(0.)).min(h);
            (ix, h, -offset)
        }
        Anchor::End => {
            let ix = count - 1;
            if used >= budget {
                return truncated_frame(viewport, count, rows, up_tmp);
            }
            let h = heights.height(ix);
            used += 1;
            // Last row's bottom sits on the viewport bottom.
            (ix, h, height - h)
        }
    };

    // C. Consume the accumulated scroll delta as a pure translation. A positive
    //    delta scrolls toward older content: the anchor row moves down the
    //    window (content appears to sink), so the viewport reveals rows above.
    let delta = state.pending_delta_y;
    state.pending_delta_y = px(0.);
    let seed_top_y = seed_top_y + delta;
    push_row(
        &mut rows,
        &mut placed_lo,
        &mut placed_hi,
        seed_ix,
        width,
        height,
        overdraw,
        viewport.origin,
        seed_top_y,
        seed_h,
    );

    // D. Fill up and down from the anchored row's neighbours. Each row is
    //    measured once; budget exhaustion stops the fill.
    let (reached_bottom, down_exhausted) = fill_down(
        seed_ix + 1,
        seed_top_y + seed_h,
        count,
        width,
        height,
        overdraw,
        viewport.origin,
        heights,
        &mut used,
        budget,
        &mut rows,
        &mut placed_lo,
        &mut placed_hi,
    );
    exhausted |= down_exhausted;
    let (reached_top, up_exhausted) = fill_up(
        seed_ix,
        seed_top_y,
        width,
        height,
        overdraw,
        viewport.origin,
        heights,
        &mut used,
        budget,
        &mut up_tmp,
        &mut placed_lo,
        &mut placed_hi,
    );
    exhausted |= up_exhausted;

    // The delta translated the anchor row far out of the viewport and the
    // budget could not measure its way back — the frame would paint nothing.
    // Snap to the content edge in the delta direction and re-plan once so the
    // viewport always shows content. `depth` bounds the recursion to a single
    // re-anchor.
    if exhausted && rows.is_empty() && up_tmp.is_empty() && depth == 0 {
        // Disengage tail-follow so the re-anchored edge position survives the
        // next frame's A phase instead of being overridden back to `End`.
        state.following_tail = false;
        state.anchor = if delta > px(0.) {
            Anchor::Top {
                key: keys[0],
                ix_hint: 0,
                offset: px(0.),
            }
        } else {
            Anchor::End
        };
        let mut plan = plan_frame_inner(
            state,
            align,
            viewport,
            overdraw,
            keys,
            heights,
            budget,
            depth + 1,
        );
        if plan.rows.is_empty() {
            // A second budget exhaustion (e.g. hundreds of thousands of
            // zero-height rows) still left nothing to paint; the anchor is now
            // at a content edge, so the next frame recovers from there.
            log::warn!("ChatList budget exhausted across a re-anchor; viewport empty");
        }
        plan.budget_exhausted = true;
        return plan;
    }

    // E. Boundary clamping. At most one extra fill runs; `shift` moves the
    //    whole placed range so the viewport settles on an edge.
    let first_top = rows
        .iter()
        .chain(up_tmp.iter())
        .min_by_key(|r| r.ix)
        .map(|r| r.slot.top() - viewport.origin.y)
        .unwrap_or(px(0.));
    let last_bottom = rows
        .iter()
        .chain(up_tmp.iter())
        .max_by_key(|r| r.ix)
        .map(|r| r.slot.bottom() - viewport.origin.y)
        .unwrap_or(px(0.));
    let content_h = last_bottom - first_top;
    let underflow = reached_top && reached_bottom && content_h <= height;

    let mut shift = px(0.);
    if underflow {
        shift = match align {
            ListAlignment::Bottom => height - last_bottom,
            ListAlignment::Top => -first_top,
        };
    } else if first_top > px(0.) {
        // Scrolled above the content top: pin the first row to the viewport
        // top, then extend downward since the bottom edge moved down.
        shift = -first_top;
        if !reached_bottom {
            let y0 = last_bottom + shift;
            let (_rb, ex) = fill_down(
                placed_hi + 1,
                y0,
                count,
                width,
                height,
                overdraw,
                viewport.origin,
                heights,
                &mut used,
                budget,
                &mut rows,
                &mut placed_lo,
                &mut placed_hi,
            );
            exhausted |= ex;
        }
    } else if last_bottom < height {
        // The viewport hangs past the content bottom; bottom-aligned content
        // snaps down to it (top-aligned content short of the viewport is
        // covered by `underflow`).
        shift = height - last_bottom;
        if !reached_top {
            let y0 = first_top + shift;
            let (_rt, ex) = fill_up(
                placed_lo.saturating_sub(1),
                y0,
                width,
                height,
                overdraw,
                viewport.origin,
                heights,
                &mut used,
                budget,
                &mut up_tmp,
                &mut placed_lo,
                &mut placed_hi,
            );
            exhausted |= ex;
        }
    }
    if shift != px(0.) {
        for row in rows.iter_mut().chain(up_tmp.iter_mut()) {
            row.slot.origin.y += shift;
        }
    }

    up_tmp.reverse();
    up_tmp.extend(rows);
    let rows = up_tmp;

    // F. Anchor write-back: the row straddling the viewport top, re-engaging
    //    tail-follow when the frame landed at the bottom.
    let first_top = rows
        .first()
        .map(|r| r.slot.top() - viewport.origin.y)
        .unwrap_or(px(0.));
    let last_bottom = rows
        .last()
        .map(|r| r.slot.bottom() - viewport.origin.y)
        .unwrap_or(px(0.));
    let at_top = reached_top;
    let at_bottom = reached_bottom && (last_bottom - height).abs() <= px(1.);
    if at_bottom {
        state.following_tail = true;
    }
    let view_top = viewport.origin.y;
    let next_anchor = rows
        .iter()
        .find(|r| r.slot.top() <= view_top && r.slot.bottom() > view_top)
        .map(|r| {
            let offset = (view_top - r.slot.top()).clamp(px(0.), r.slot.size.height);
            Anchor::Top {
                key: keys[r.ix],
                ix_hint: r.ix,
                offset,
            }
        })
        .or_else(|| {
            rows.first().map(|r| Anchor::Top {
                key: keys[r.ix],
                ix_hint: r.ix,
                offset: first_top.max(px(0.)).min(r.slot.size.height),
            })
        })
        .unwrap_or(state.anchor);
    state.anchor = next_anchor;

    let placed = if placed_lo <= placed_hi {
        placed_lo..placed_hi + 1
    } else {
        0..0
    };
    let visible = visible_range(&rows, viewport);
    let report = FrameReport {
        viewport,
        placed,
        visible,
        at_top,
        at_bottom,
        underflow,
        count,
    };

    FramePlan {
        rows,
        report,
        // The delta is fully consumed this frame; budget exhaustion re-anchors
        // to the content edge, so no delta needs re-queueing — only a repaint.
        remaining_delta_y: px(0.),
        budget_exhausted: exhausted,
    }
}

fn truncated_frame(
    viewport: Bounds<Pixels>,
    count: usize,
    mut rows: Vec<PlacedRow>,
    mut up_tmp: Vec<PlacedRow>,
) -> FramePlan {
    up_tmp.reverse();
    up_tmp.extend(rows);
    rows = up_tmp;
    FramePlan {
        rows,
        report: FrameReport {
            viewport,
            placed: 0..0,
            visible: 0..0,
            at_top: false,
            at_bottom: false,
            underflow: false,
            count,
        },
        remaining_delta_y: px(0.),
        budget_exhausted: true,
    }
}

// Row geometry is a hot leaf of `plan_frame`; the flat parameter list mirrors
// the fill helpers that call it.
#[allow(clippy::too_many_arguments)]
fn push_row(
    out: &mut Vec<PlacedRow>,
    placed_lo: &mut usize,
    placed_hi: &mut usize,
    ix: usize,
    width: Pixels,
    height: Pixels,
    overdraw: Pixels,
    origin: Point<Pixels>,
    top_y: Pixels,
    h: Pixels,
) {
    if top_y + h > -overdraw && top_y < height + overdraw {
        out.push(PlacedRow {
            ix,
            slot: Bounds::new(point(origin.x, origin.y + top_y), size(width, h)),
        });
        *placed_lo = (*placed_lo).min(ix);
        *placed_hi = (*placed_hi).max(ix);
    }
}

#[allow(clippy::too_many_arguments)]
fn fill_down(
    ix0: usize,
    y0: Pixels,
    count: usize,
    width: Pixels,
    height: Pixels,
    overdraw: Pixels,
    origin: Point<Pixels>,
    heights: &mut dyn HeightOracle,
    used: &mut usize,
    budget: usize,
    out: &mut Vec<PlacedRow>,
    placed_lo: &mut usize,
    placed_hi: &mut usize,
) -> (bool, bool) {
    let mut ix = ix0;
    let mut y = y0;
    loop {
        if ix >= count {
            return (true, false);
        }
        if *used >= budget {
            return (false, true);
        }
        let h = heights.height(ix);
        *used += 1;
        push_row(
            out, placed_lo, placed_hi, ix, width, height, overdraw, origin, y, h,
        );
        y += h;
        ix += 1;
        if y >= height + overdraw {
            return (false, false);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn fill_up(
    ix0: usize,
    y0: Pixels,
    width: Pixels,
    height: Pixels,
    overdraw: Pixels,
    origin: Point<Pixels>,
    heights: &mut dyn HeightOracle,
    used: &mut usize,
    budget: usize,
    out: &mut Vec<PlacedRow>,
    placed_lo: &mut usize,
    placed_hi: &mut usize,
) -> (bool, bool) {
    let mut ix = ix0;
    let mut y = y0;
    loop {
        if *used >= budget {
            return (false, true);
        }
        if ix == 0 {
            return (true, false);
        }
        let h = heights.height(ix - 1);
        *used += 1;
        y -= h;
        push_row(
            out,
            placed_lo,
            placed_hi,
            ix - 1,
            width,
            height,
            overdraw,
            origin,
            y,
            h,
        );
        ix -= 1;
        if y <= -overdraw {
            return (false, false);
        }
    }
}

/// Row indices whose slots intersect the viewport, in ascending order.
fn visible_range(rows: &[PlacedRow], viewport: Bounds<Pixels>) -> Range<usize> {
    let top = viewport.origin.y;
    let bottom = viewport.origin.y + viewport.size.height;
    let lo = rows
        .iter()
        .find(|r| r.slot.bottom() > top && r.slot.top() < bottom)
        .map(|r| r.ix)
        .unwrap_or(0);
    let hi = rows
        .iter()
        .rev()
        .find(|r| r.slot.bottom() > top && r.slot.top() < bottom)
        .map(|r| r.ix + 1)
        .unwrap_or(lo);
    lo..hi
}

/// Resolve an anchored row key to an index. `ix_hint` hit is O(1) — the steady
/// state for an append-only chat; otherwise a ±64 local sweep, then a full
/// scan, then a fallback to `min(ix_hint, count-1)` (clamped by the frame's
/// edge logic). Never panics.
fn resolve_key(keys: &[RowKey], key: RowKey, ix_hint: usize) -> usize {
    if ix_hint < keys.len() && keys[ix_hint] == key {
        return ix_hint;
    }
    let lo = ix_hint.saturating_sub(64);
    let hi = (ix_hint + 64).min(keys.len());
    if let Some(offset) = keys[lo..hi].iter().position(|k| *k == key) {
        return lo + offset;
    }
    if let Some(ix) = keys.iter().position(|k| *k == key) {
        return ix;
    }
    ix_hint.min(keys.len().saturating_sub(1))
}

/// The row renderer. `keys` mirrors the caller's item list one-to-one; `render`
/// must stay a read-only projection of the conversation (mutating the subtree
/// during measurement double-leases the Workspace — see #510).
pub struct ChatList {
    state: ChatListState,
    keys: Vec<RowKey>,
    render: Box<RowRenderer>,
    style: StyleRefinement,
}

impl ChatList {
    pub fn new(
        state: ChatListState,
        keys: Vec<RowKey>,
        render: impl FnMut(usize, &mut Window, &mut App) -> gpui::AnyElement + 'static,
    ) -> Self {
        Self {
            state,
            keys,
            render: Box::new(render),
            style: StyleRefinement::default(),
        }
    }
}

impl IntoElement for ChatList {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Styled for ChatList {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

/// One placed row ready to paint: already measured and pre-painted at its
/// slot, whose bounds double as its paint-time content mask.
pub struct PendingRow {
    element: gpui::AnyElement,
    slot: Bounds<Pixels>,
}

pub struct Prepaint {
    rows: Vec<PendingRow>,
    hitbox_id: HitboxId,
}

/// Measures rows through `layout_as_root` and keeps the measured elements for
/// the placement pass — so each row is rendered and measured once per frame
/// and painted with the geometry it actually reported.
struct FrameOracle<'a> {
    window: &'a mut Window,
    cx: &'a mut App,
    width: Pixels,
    render: &'a mut RowRenderer,
    elements: Vec<Option<gpui::AnyElement>>,
}

impl HeightOracle for FrameOracle<'_> {
    fn height(&mut self, ix: usize) -> Pixels {
        if self.elements.len() <= ix {
            self.elements.resize_with(ix + 1, || None);
        }
        if self.elements[ix].is_none() {
            self.elements[ix] = Some((self.render)(ix, self.window, self.cx));
        }
        self.elements[ix]
            .as_mut()
            .unwrap()
            .layout_as_root(
                Size::new(
                    AvailableSpace::Definite(self.width),
                    AvailableSpace::MinContent,
                ),
                self.window,
                self.cx,
            )
            .height
    }
}

impl Element for ChatList {
    type RequestLayoutState = ();
    type PrepaintState = Prepaint;

    fn id(&self) -> Option<ElementId> {
        // No cross-frame element state; the anchor lives on the shared handle
        // so it can be rewritten by event handlers outside paint frames.
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
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.refine(&self.style);
        // Nothing is measured here: the only measurement this list ever does is
        // a definite-width one during prepaint, so there is exactly one width
        // semantics per frame.
        let layout_id = window.request_layout(style, [], cx);
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);

        let mut inner = self.state.0.borrow_mut();
        let mut state = inner.state;
        let align = inner.align;
        let overdraw = inner.overdraw;

        let mut oracle = FrameOracle {
            window,
            cx,
            width: bounds.size.width,
            render: self.render.as_mut(),
            elements: Vec::new(),
        };
        let plan = plan_frame(
            &mut state,
            align,
            bounds,
            overdraw,
            &self.keys,
            &mut oracle,
            MAX_MEASURES_PER_FRAME,
        );

        inner.state = state;
        inner.last = Some(plan.report.clone());

        // Measure-then-place: every placed row is pre-painted before any of
        // them paints — `layout_as_root` on an already-pre-painted element
        // panics, so the two passes never interleave. The oracle's borrow of
        // `window`/`cx` must end before placement can borrow them again, so the
        // measured elements are moved out first.
        let mut measured = std::mem::take(&mut oracle.elements);
        drop(oracle);
        let mut rows = Vec::with_capacity(plan.rows.len());
        for row in plan.rows {
            let Some(mut element) = measured.get_mut(row.ix).and_then(|e| e.take()) else {
                continue;
            };
            element.prepaint_at(row.slot.origin, window, cx);
            rows.push(PendingRow {
                element,
                slot: row.slot,
            });
        }

        if plan.budget_exhausted
            && let Some(view) = inner.view
        {
            cx.notify(view);
        }

        Prepaint {
            rows,
            hitbox_id: hitbox.id,
        }
    }

    fn paint(
        &mut self,
        _global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        // Per-row content mask: any residual mismatch between the height a row
        // reported and the height it paints clips that row to its own slot
        // instead of overlapping its neighbour.
        for row in prepaint.rows.iter_mut() {
            window.with_content_mask(Some(ContentMask { bounds: row.slot }), |window| {
                row.element.paint(window, cx);
            });
        }

        // Scroll is accumulated as pixels and applied as a pure translation at
        // the next prepaint. Measuring here is impossible — during event
        // dispatch the draw phase is not Prepaint, so `layout_as_root` would
        // trip the prepaint assertion.
        let state = self.state.clone();
        let hitbox_id = prepaint.hitbox_id;
        window.on_mouse_event(move |event: &ScrollWheelEvent, phase, window, cx| {
            if phase == DispatchPhase::Bubble && hitbox_id.should_handle_scroll(window) {
                let mut inner = state.0.borrow_mut();
                let dy = event.delta.pixel_delta(window.line_height()).y;
                if dy > px(0.) {
                    inner.state.following_tail = false;
                }
                inner.state.pending_delta_y += dy;
                if let Some(view) = inner.view {
                    cx.notify(view);
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed-height row source so boundary branches are testable without a window.
    struct Fixed {
        heights: Vec<Pixels>,
    }

    impl HeightOracle for Fixed {
        fn height(&mut self, ix: usize) -> Pixels {
            self.heights[ix]
        }
    }

    fn keys(n: usize) -> Vec<RowKey> {
        (0..n)
            .map(|i| RowKey::from_entity_id(gpui::EntityId::from(i as u64)))
            .collect()
    }

    fn viewport(h: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(0.), px(0.)), size(px(480.), px(h)))
    }

    fn run(state: &mut State, heights: &[Pixels], viewport_h: f32, budget: usize) -> FramePlan {
        plan_frame(
            state,
            ListAlignment::Bottom,
            viewport(viewport_h),
            px(0.),
            &keys(heights.len()),
            &mut Fixed {
                heights: heights.to_vec(),
            },
            budget,
        )
    }

    fn heights_n(n: usize, h: f32) -> Vec<Pixels> {
        vec![px(h); n]
    }

    #[test]
    fn underflow_sits_at_bottom() {
        // 10 rows of 20px in a 300px viewport: content is shorter than the
        // viewport, so the whole list sits at the bottom.
        let mut state = State::default();
        let plan = run(&mut state, &heights_n(10, 20.), 300., 512);
        assert!(plan.report.underflow);
        assert!(plan.report.at_bottom);
        assert_eq!(plan.rows.len(), 10);
        let first = plan.rows.iter().find(|r| r.ix == 0).unwrap();
        assert_eq!(first.slot.top(), px(100.));
        let last = plan.rows.iter().find(|r| r.ix == 9).unwrap();
        assert_eq!(last.slot.bottom(), px(300.));
    }

    #[test]
    fn top_aligned_underflow_sits_at_top() {
        let mut state = State::default();
        let plan = plan_frame(
            &mut state,
            ListAlignment::Top,
            viewport(300.),
            px(0.),
            &keys(10),
            &mut Fixed {
                heights: heights_n(10, 20.),
            },
            512,
        );
        assert!(plan.report.underflow);
        let first = plan.rows.iter().find(|r| r.ix == 0).unwrap();
        assert_eq!(first.slot.top(), px(0.));
    }

    #[test]
    fn anchor_writeback_invariant_holds() {
        // End-anchored scroll over 100 rows: the anchor re-expresses the row
        // straddling the viewport top with a clamped offset.
        let mut state = State::default();
        let _ = run(&mut state, &heights_n(100, 20.), 300., 512);
        let Anchor::Top {
            offset, ix_hint, ..
        } = state.anchor
        else {
            panic!("expected Top anchor, got End");
        };
        assert_eq!(ix_hint, 85);
        assert!(offset >= px(0.));
        assert!(offset < px(20.));
    }

    #[test]
    fn deleted_anchor_row_clamps_and_scrolls_forward() {
        let mut state = State::default();
        let keys = keys(100);
        state.anchor = Anchor::Top {
            key: keys[50],
            ix_hint: 50,
            offset: px(5.),
        };
        let mut kept = keys.clone();
        kept.remove(50);
        let plan = plan_frame(
            &mut state,
            ListAlignment::Bottom,
            viewport(300.),
            px(0.),
            &kept,
            &mut Fixed {
                heights: heights_n(99, 20.),
            },
            512,
        );
        assert!(!plan.rows.is_empty());
    }

    #[test]
    fn tail_overrides_pending_row_scroll() {
        let mut state = State {
            following_tail: true,
            pending: Some(PendingScroll::ToRow(keys(100)[50])),
            ..State::default()
        };
        let plan = run(&mut state, &heights_n(100, 20.), 300., 512);
        assert!(plan.report.at_bottom);
    }

    #[test]
    fn ten_thousand_zero_height_rows_terminate() {
        let mut state = State::default();
        let plan = run(&mut state, &heights_n(10_000, 0.), 300., 512);
        // Zero-height rows all pile at the viewport bottom, so either the whole
        // content fits (`underflow`) or the fill budget capped how far up the
        // degenerate rows were walked (`budget_exhausted`) — both terminate.
        assert!(plan.report.underflow || plan.budget_exhausted);
    }

    #[test]
    fn budget_exhaustion_reanchors_to_content_edge() {
        // A huge upward delta must not dead-lock or paint a blank viewport:
        // the frame runs out of budget and re-anchors to the content top so
        // rows are still visible.
        let mut state = State {
            pending_delta_y: px(100_000.),
            ..State::default()
        };
        let plan = run(&mut state, &heights_n(10_000, 20.), 300., 20);
        assert!(plan.budget_exhausted);
        assert_eq!(plan.remaining_delta_y, px(0.));
        assert!(!plan.rows.is_empty());
        assert_eq!(plan.report.visible.start, 0);
    }

    #[test]
    fn same_row_measured_once_per_frame() {
        let mut state = State::default();
        let mut oracle = CountingOracle {
            heights: heights_n(100, 20.),
            calls: 0,
        };
        plan_frame(
            &mut state,
            ListAlignment::Bottom,
            viewport(300.),
            px(0.),
            &keys(100),
            &mut oracle,
            512,
        );
        // Seed row + the 15 visible rows below it, each measured exactly once.
        assert_eq!(oracle.calls, 16);
    }

    #[test]
    fn measurements_independent_of_list_length() {
        assert_eq!(measure_count(100), measure_count(100_000));
    }

    fn measure_count(n: usize) -> usize {
        let mut state = State::default();
        let mut oracle = CountingOracle {
            heights: heights_n(n, 20.),
            calls: 0,
        };
        plan_frame(
            &mut state,
            ListAlignment::Bottom,
            viewport(300.),
            px(0.),
            &keys(n),
            &mut oracle,
            512,
        );
        oracle.calls
    }

    struct CountingOracle {
        heights: Vec<Pixels>,
        calls: usize,
    }

    impl HeightOracle for CountingOracle {
        fn height(&mut self, ix: usize) -> Pixels {
            self.calls += 1;
            self.heights[ix]
        }
    }

    #[test]
    fn empty_list_leaves_anchor_untouched() {
        let mut state = State {
            pending: Some(PendingScroll::ToEnd),
            ..State::default()
        };
        let plan = run(&mut state, &[], 300., 512);
        assert!(plan.rows.is_empty());
        // Pending survives to fire once a real list exists.
        assert!(state.pending.is_some());
    }

    #[test]
    fn zero_viewport_leaves_anchor_untouched() {
        let mut state = State {
            pending: Some(PendingScroll::ToEnd),
            ..State::default()
        };
        let plan = plan_frame(
            &mut state,
            ListAlignment::Bottom,
            viewport(0.),
            px(0.),
            &keys(10),
            &mut Fixed {
                heights: heights_n(10, 20.),
            },
            512,
        );
        assert!(plan.rows.is_empty());
        assert!(state.pending.is_some());
    }

    #[test]
    fn scroll_disengages_tail_and_scroll_to_row_repositions() {
        let mut state = State::default();
        let _ = run(&mut state, &heights_n(100, 20.), 300., 512);
        assert!(state.following_tail);
        // An upward scroll disengages tail-follow.
        state.following_tail = false;
        state.pending_delta_y = px(50.);
        let plan = run(&mut state, &heights_n(100, 20.), 300., 512);
        assert!(!plan.report.at_bottom);
        // scroll_to_row repositions and stays off the tail.
        state.pending = Some(PendingScroll::ToRow(keys(100)[50]));
        let plan = run(&mut state, &heights_n(100, 20.), 300., 512);
        assert_eq!(plan.report.visible.start, 50);
        assert!(!state.following_tail);
        // Landing back at the bottom re-engages.
        state.pending = Some(PendingScroll::ToEnd);
        state.following_tail = false;
        let plan = run(&mut state, &heights_n(100, 20.), 300., 512);
        assert!(plan.report.at_bottom);
        assert!(state.following_tail);
    }

    #[test]
    fn scroll_to_row_survives_insertion_before_target() {
        let mut state = State::default();
        let keys = keys(100);
        state.anchor = Anchor::Top {
            key: keys[50],
            ix_hint: 50,
            offset: px(0.),
        };
        // Insert a row before the target: the key still resolves to index 51.
        state.following_tail = false;
        let mut new_keys = Vec::new();
        new_keys.push(RowKey::from_entity_id(gpui::EntityId::from(10_000)));
        new_keys.extend(keys.iter().copied());
        let plan = plan_frame(
            &mut state,
            ListAlignment::Bottom,
            viewport(300.),
            px(0.),
            &new_keys,
            &mut Fixed {
                heights: heights_n(101, 20.),
            },
            512,
        );
        assert_eq!(plan.report.visible.start, 51);
    }

    #[test]
    fn resolve_key_fallback_clamps_to_range() {
        let ks = keys(5);
        assert_eq!(resolve_key(&ks, ks[3], 3), 3);
        assert_eq!(resolve_key(&ks, ks[3], 0), 3);
        assert_eq!(
            resolve_key(&ks, RowKey::from_entity_id(gpui::EntityId::from(999)), 4),
            4
        );
    }

    #[test]
    fn adjacent_rows_share_boundaries() {
        let mut state = State::default();
        let plan = run(&mut state, &heights_n(100, 20.), 300., 512);
        for pair in plan.rows.windows(2) {
            let a = &pair[0];
            let b = &pair[1];
            assert_eq!(a.ix + 1, b.ix);
            let gap = (a.slot.bottom() - b.slot.top()).abs();
            assert!(gap <= px(1.0), "gap between rows {a:?} and {b:?}: {gap:?}");
        }
    }
}
