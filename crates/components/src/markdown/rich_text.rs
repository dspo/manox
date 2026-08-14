//! `RichText` — Manox-owned shaping, painting and document selection.
//!
//! GPUI's old `StyledText` cache is intentionally not used here. It keyed
//! wrapped layouts incompletely, so a min-content or transient zero-width
//! probe could be reused for a later definite-width paint. In a virtual list
//! that manifests as either an enormous cached row (blank screen) or glyphs
//! painting beyond their allocated row (overlap).
//!
//! This leaf uses only GPUI's public measured-layout and text-system APIs. Each
//! measure call shapes for its own constraint, unusably narrow definite widths
//! are treated as intrinsic probes, and prepaint verifies the shaped width
//! against the final allocated width before any glyph is painted.

use std::{cell::RefCell, ops::Range, rc::Rc};

use gpui::{
    App, AvailableSpace, Bounds, Element, GlobalElementId, HighlightStyle, Hsla,
    InspectorElementId, IntoElement, LayoutId, Pixels, SharedString, Size, Style, TextRun,
    TextStyle, WhiteSpace, Window, WrappedLine, fill, point, px, size,
};

use crate::markdown::ast::LinkSpan;
use crate::markdown::selection::{BlockHit, BlockLayout, DocSelection};

#[derive(Clone)]
pub struct CodeSpan {
    pub range: Range<usize>,
    pub fg: Hsla,
}

pub struct RichText {
    text: SharedString,
    highlights: Vec<(Range<usize>, HighlightStyle)>,
    code_spans: Vec<CodeSpan>,
    doc_start: usize,
    selection: DocSelection,
    selection_bg: Hsla,
    join_before: &'static str,
    link_spans: Vec<LinkSpan>,
    link_color: Hsla,
}

impl RichText {
    pub fn new(text: impl Into<SharedString>, doc_start: usize, selection: DocSelection) -> Self {
        Self {
            text: text.into(),
            highlights: Vec::new(),
            code_spans: Vec::new(),
            doc_start,
            selection,
            selection_bg: Hsla::default(),
            join_before: "\n\n",
            link_spans: Vec::new(),
            link_color: Hsla::default(),
        }
    }

    pub fn highlights(mut self, highlights: Vec<(Range<usize>, HighlightStyle)>) -> Self {
        self.highlights = highlights;
        self
    }

    pub fn code_spans(mut self, spans: Vec<CodeSpan>) -> Self {
        self.code_spans = spans;
        self
    }

    pub fn selection_bg(mut self, bg: Hsla) -> Self {
        self.selection_bg = bg;
        self
    }

    pub fn join_before(mut self, sep: &'static str) -> Self {
        self.join_before = sep;
        self
    }

    pub fn link_spans(mut self, spans: Vec<LinkSpan>) -> Self {
        self.link_spans = spans;
        self
    }

    pub fn link_color(mut self, color: Hsla) -> Self {
        self.link_color = color;
        self
    }
}

fn merge_code_highlights(
    highlights: &mut Vec<(Range<usize>, HighlightStyle)>,
    code_spans: &[CodeSpan],
) {
    for span in code_spans {
        let code_highlight = HighlightStyle {
            color: Some(span.fg),
            ..Default::default()
        };
        if let Some(existing) = highlights
            .iter_mut()
            .find(|(range, _)| range == &span.range)
        {
            existing.1.color = code_highlight.color;
        } else {
            let pos = highlights
                .iter()
                .position(|(range, _)| range.start > span.range.start)
                .unwrap_or(highlights.len());
            highlights.insert(pos, (span.range.clone(), code_highlight));
        }
    }
}

fn compute_runs(
    text: &str,
    default_style: &TextStyle,
    highlights: &[(Range<usize>, HighlightStyle)],
) -> Vec<TextRun> {
    let mut runs = Vec::new();
    let mut ix = 0;
    for (range, highlight) in highlights {
        if ix < range.start {
            runs.push(default_style.to_run(range.start - ix));
        }
        runs.push(
            default_style
                .clone()
                .highlight(*highlight)
                .to_run(range.len()),
        );
        ix = range.end;
    }
    if ix < text.len() {
        runs.push(default_style.to_run(text.len() - ix));
    }
    runs
}

struct ShapedText {
    lines: Rc<Vec<WrappedLine>>,
    size: Size<Pixels>,
    wrap_width: Option<Pixels>,
    line_height: Pixels,
    bounds: Option<Bounds<Pixels>>,
}

#[derive(Default)]
struct ShapeCache {
    /// Layout selected by the most recent non-probe measurement. Intrinsic
    /// min/max-content probes must never replace this: Taffy may issue them in
    /// either order before resolving the width used for paint.
    paint: Option<ShapedText>,
    min_content_size: Option<Size<Pixels>>,
    max_content_size: Option<Size<Pixels>>,
}

#[cfg(test)]
thread_local! {
    static LAST_MEASURED_SIZE: RefCell<Option<Size<Pixels>>> = const { RefCell::new(None) };
}

pub struct RichTextState {
    shaped: Rc<RefCell<ShapeCache>>,
    text: SharedString,
    runs: Vec<TextRun>,
    font_size: Pixels,
    line_height: Pixels,
    text_style: TextStyle,
}

/// Mirror GPUI's wrap-candidate classification at the pinned revision. It is
/// intentionally local because `LineWrapper::is_word_char` is not public.
fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(c, '\u{00C0}'..='\u{00FF}')
        || matches!(c, '\u{0100}'..='\u{017F}')
        || matches!(c, '\u{0180}'..='\u{024F}')
        || matches!(c, '\u{0400}'..='\u{04FF}')
        || matches!(c, '\u{1E00}'..='\u{1EFF}')
        || matches!(c, '\u{0300}'..='\u{036F}')
        || matches!(c, '\u{0980}'..='\u{09FF}')
        || matches!(
            c,
            '-' | '_'
                | '.'
                | '\''
                | '’'
                | '‘'
                | '$'
                | '%'
                | '@'
                | '#'
                | '^'
                | '~'
                | ','
                | '='
                | ':'
                | ';'
                | '⋯'
        )
}

/// Width of the widest segment that GPUI's line wrapper cannot break. This is
/// the honest CSS min-content width used for intrinsic probes; returning the
/// full unwrapped width here pins flex cells, while returning a tiny width
/// inflates their probe height and can leave a large blank list row.
fn min_content_width(lines: &[WrappedLine]) -> Pixels {
    lines.iter().fold(px(0.), |widest, line| {
        let mut max_segment_width = px(0.);
        let mut segment_start_x = px(0.);
        let mut first_non_whitespace_ix = None;
        let mut prev_ch = '\0';
        let mut glyphs = line
            .unwrapped_layout
            .runs
            .iter()
            .flat_map(|run| run.glyphs.iter())
            .peekable();

        while let Some(glyph) = glyphs.next() {
            let ch = line.text[glyph.index..].chars().next().unwrap();
            if ch == '\n' {
                continue;
            }
            let is_break_candidate = if is_word_char(ch) {
                prev_ch == ' ' && ch != ' ' && first_non_whitespace_ix.is_some()
            } else {
                ch != ' ' && first_non_whitespace_ix.is_some()
            };
            if is_break_candidate {
                segment_start_x = glyph.position.x;
            }
            if ch != ' ' && first_non_whitespace_ix.is_none() {
                first_non_whitespace_ix = Some(glyph.index);
            }
            if ch != ' ' {
                let glyph_end_x = glyphs
                    .peek()
                    .map_or(line.unwrapped_layout.width, |glyph| glyph.position.x);
                max_segment_width = max_segment_width.max(glyph_end_x - segment_start_x);
            }
            prev_ch = ch;
        }
        widest.max(max_segment_width)
    })
}

fn useful_wrap_width(
    known_width: Option<Pixels>,
    available_width: AvailableSpace,
    font_size: Pixels,
    white_space: WhiteSpace,
) -> Option<Pixels> {
    if white_space != WhiteSpace::Normal {
        return None;
    }
    let width = known_width.or(match available_width {
        AvailableSpace::Definite(width) => Some(width),
        AvailableSpace::MinContent | AvailableSpace::MaxContent => None,
    })?;
    // A width narrower than one em is a transient flex/intrinsic probe, not a
    // useful message-column width. Character-wrapping at 0–1px can turn a
    // paragraph into tens of thousands of pixels and poison list height caches.
    (width >= font_size.max(px(1.))).then_some(width)
}

fn shape(
    text: SharedString,
    runs: &[TextRun],
    font_size: Pixels,
    line_height: Pixels,
    wrap_width: Option<Pixels>,
    line_clamp: Option<usize>,
    window: &mut Window,
) -> ShapedText {
    let lines = window
        .text_system()
        .shape_text(text, font_size, runs, wrap_width, line_clamp)
        .map(|lines| Rc::new(lines.into_iter().collect::<Vec<_>>()))
        .unwrap_or_else(|error| {
            log::error!("failed to shape markdown RichText: {error:#}");
            Rc::default()
        });
    let mut measured: Size<Pixels> = Size::default();
    for line in lines.iter() {
        let line_size = line.size(line_height);
        measured.width = measured.width.max(line_size.width).ceil();
        measured.height += line_size.height;
    }
    ShapedText {
        lines,
        size: measured,
        wrap_width,
        line_height,
        bounds: None,
    }
}

#[allow(clippy::too_many_arguments)]
fn measure_shape(
    cache: &Rc<RefCell<ShapeCache>>,
    known_width: Option<Pixels>,
    available_width: AvailableSpace,
    text: &SharedString,
    runs: &[TextRun],
    text_style: &TextStyle,
    font_size: Pixels,
    line_height: Pixels,
    window: &mut Window,
) -> Size<Pixels> {
    let wrap_width = useful_wrap_width(
        known_width,
        available_width,
        font_size,
        text_style.white_space,
    );

    // Taffy asks intrinsic min/max-content questions while flex widths are
    // unresolved. Their answers participate in parent sizing, but must not
    // replace the definite-width lines later used for paint. A sub-em definite
    // probe is routed through the max-content slot for the same reason.
    if text_style.white_space == WhiteSpace::Normal && wrap_width.is_none() {
        let is_min_content =
            known_width.is_none() && matches!(available_width, AvailableSpace::MinContent);
        let cached = {
            let cache = cache.borrow();
            if is_min_content {
                cache.min_content_size
            } else {
                cache.max_content_size
            }
        };
        if let Some(size) = cached {
            #[cfg(test)]
            LAST_MEASURED_SIZE.with(|slot| slot.replace(Some(size)));
            return size;
        }

        let unwrapped = shape(
            text.clone(),
            runs,
            font_size,
            line_height,
            None,
            text_style.line_clamp,
            window,
        );
        let next = if is_min_content {
            let width = min_content_width(&unwrapped.lines);
            shape(
                text.clone(),
                runs,
                font_size,
                line_height,
                Some(width),
                text_style.line_clamp,
                window,
            )
        } else {
            unwrapped
        };
        let size = next.size;
        let mut cache = cache.borrow_mut();
        if is_min_content {
            cache.min_content_size = Some(size);
        } else {
            cache.max_content_size = Some(size);
        }
        // An unconstrained element may never receive a definite measure. Keep
        // the first probe as a paint fallback only; any later definite-width
        // call replaces it.
        if cache.paint.is_none() {
            cache.paint = Some(next);
        }
        #[cfg(test)]
        LAST_MEASURED_SIZE.with(|slot| slot.replace(Some(size)));
        return size;
    }

    if let Some(size) = cache
        .borrow()
        .paint
        .as_ref()
        .filter(|layout| layout.wrap_width == wrap_width)
        .map(|layout| layout.size)
    {
        #[cfg(test)]
        LAST_MEASURED_SIZE.with(|slot| slot.replace(Some(size)));
        return size;
    }
    let next = shape(
        text.clone(),
        runs,
        font_size,
        line_height,
        wrap_width,
        text_style.line_clamp,
        window,
    );
    let size = next.size;
    #[cfg(test)]
    LAST_MEASURED_SIZE.with(|slot| slot.replace(Some(size)));
    cache.borrow_mut().paint = Some(next);
    size
}

impl IntoElement for RichText {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for RichText {
    type RequestLayoutState = RichTextState;
    type PrepaintState = ();

    fn id(&self) -> Option<gpui::ElementId> {
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
        let mut highlights = self.highlights.clone();
        merge_code_highlights(&mut highlights, &self.code_spans);
        let text_style = window.text_style();
        let font_size = text_style.font_size.to_pixels(window.rem_size());
        let line_height = window.pixel_snap(
            text_style
                .line_height
                .to_pixels(font_size.into(), window.rem_size()),
        );
        let runs = compute_runs(&self.text, &text_style, &highlights);
        let text = self.text.clone();
        let measure_text = text.clone();
        let measure_runs = runs.clone();
        let measure_style = text_style.clone();
        let shaped = Rc::new(RefCell::new(ShapeCache::default()));
        let measured = shaped.clone();
        let layout_id = window.request_measured_layout(
            Style::default(),
            move |known, available, window, _cx| {
                measure_shape(
                    &measured,
                    known.width,
                    available.width,
                    &measure_text,
                    &measure_runs,
                    &measure_style,
                    font_size,
                    line_height,
                    window,
                )
            },
        );
        (
            layout_id,
            RichTextState {
                shaped,
                text,
                runs,
                font_size,
                line_height,
                text_style,
            },
        )
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        state: &mut Self::RequestLayoutState,
        window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
        let current_width = state
            .shaped
            .borrow()
            .paint
            .as_ref()
            .and_then(|layout| layout.wrap_width);
        let final_width = useful_wrap_width(
            Some(bounds.size.width),
            AvailableSpace::Definite(bounds.size.width),
            state.font_size,
            state.text_style.white_space,
        );
        if final_width != current_width {
            let replacement = shape(
                state.text.clone(),
                &state.runs,
                state.font_size,
                state.line_height,
                final_width,
                state.text_style.line_clamp,
                window,
            );
            // Taffy has already allocated `bounds` from the measurement above.
            // A final-width reconciliation may reduce height immediately, but
            // must never paint a taller layout into that already-allocated row.
            // The next frame will measure the definite width normally.
            if replacement.size.height <= bounds.size.height + px(1.) {
                state.shaped.borrow_mut().paint = Some(replacement);
            }
        }
        state
            .shaped
            .borrow_mut()
            .paint
            .as_mut()
            .expect("RichText measurement did not produce a paint layout")
            .bounds = Some(bounds);
    }

    fn paint(
        &mut self,
        _global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        state: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let cache = state.shaped.borrow();
        let shaped = cache
            .paint
            .as_ref()
            .expect("RichText measurement did not produce a paint layout");
        debug_assert!(
            shaped.size.height <= bounds.size.height + px(1.),
            "RichText painted height {:?} exceeds its allocated bounds {:?}; this would overlap the next message row",
            shaped.size.height,
            bounds,
        );
        let layout = BlockLayout::new(
            self.text.to_string(),
            shaped.lines.clone(),
            shaped.line_height,
            shaped.bounds.unwrap_or(bounds),
        );
        self.selection.register(BlockHit {
            doc_start: self.doc_start,
            layout: layout.clone(),
            join_before: self.join_before,
            code_ranges: self
                .code_spans
                .iter()
                .map(|span| span.range.clone())
                .collect(),
            link_spans: self.link_spans.clone(),
        });

        let block_len = layout.len();
        if let Some((s, e)) = self.selection.range() {
            let lo = s.saturating_sub(self.doc_start).min(block_len);
            let hi = e.saturating_sub(self.doc_start).min(block_len);
            if lo < hi {
                for quad in span_quads(&layout, lo, hi, px(0.)) {
                    window.paint_quad(fill(quad, self.selection_bg));
                }
            }
        }
        if self.link_color.a > 0.0 {
            for link in &self.link_spans {
                for quad in span_quads(&layout, link.range.start, link.range.end, px(0.)) {
                    let y = quad.bottom() - px(2.);
                    window.paint_quad(fill(
                        Bounds::new(point(quad.left(), y), size(quad.size.width, px(1.))),
                        self.link_color,
                    ));
                }
            }
        }

        let mut origin = bounds.origin;
        for line in shaped.lines.iter() {
            let line_bounds = Some(Bounds::new(
                origin,
                size(bounds.size.width, line.size(shaped.line_height).height),
            ));
            if let Err(error) = line.paint_background(
                origin,
                shaped.line_height,
                state.text_style.text_align,
                line_bounds,
                window,
                cx,
            ) {
                log::error!("failed to paint markdown RichText background: {error:#}");
            }
            if let Err(error) = line.paint(
                origin,
                shaped.line_height,
                state.text_style.text_align,
                line_bounds,
                window,
                cx,
            ) {
                log::error!("failed to paint markdown RichText: {error:#}");
            }
            origin.y += line.size(shaped.line_height).height;
        }
    }
}

fn span_quads(
    layout: &BlockLayout,
    start_ix: usize,
    end_ix: usize,
    pad_x: Pixels,
) -> Vec<Bounds<Pixels>> {
    let mut out = Vec::new();
    let Some(start) = layout.position_for_index(start_ix) else {
        return out;
    };
    let Some(end) = layout.position_for_index(end_ix) else {
        return out;
    };
    let line_height = layout.line_height();
    let bounds = layout.bounds();
    if ((start.y - end.y) / line_height).abs() < 0.01 {
        out.push(Bounds::new(
            point(start.x - pad_x, start.y),
            size(end.x - start.x + pad_x * 2., line_height),
        ));
    } else {
        let start_line = (((start.y - bounds.top()) / line_height).round()) as i32;
        let end_line = (((end.y - bounds.top()) / line_height).round()) as i32;
        for line in start_line..=end_line {
            let y = bounds.top() + line as f32 * line_height;
            let left = if line == start_line {
                start.x - pad_x
            } else {
                bounds.left()
            };
            let right = if line == end_line {
                end.x + pad_x
            } else {
                bounds.right()
            };
            out.push(Bounds::new(point(left, y), size(right - left, line_height)));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext, div, point};

    struct Empty;

    impl gpui::Render for Empty {
        fn render(
            &mut self,
            _window: &mut Window,
            _cx: &mut gpui::Context<Self>,
        ) -> impl IntoElement {
            div()
        }
    }

    fn blue() -> Hsla {
        gpui::hsla(0.6, 0.8, 0.5, 1.0)
    }

    #[test]
    fn unusably_narrow_width_is_an_intrinsic_probe() {
        assert_eq!(
            useful_wrap_width(
                None,
                AvailableSpace::Definite(px(0.)),
                px(13.),
                WhiteSpace::Normal
            ),
            None
        );
        assert_eq!(
            useful_wrap_width(
                None,
                AvailableSpace::Definite(px(1.)),
                px(13.),
                WhiteSpace::Normal
            ),
            None
        );
        assert_eq!(
            useful_wrap_width(
                None,
                AvailableSpace::Definite(px(480.)),
                px(13.),
                WhiteSpace::Normal
            ),
            Some(px(480.))
        );
    }

    #[test]
    fn merge_code_same_range_preserves_emphasis() {
        let mut highlights = vec![(
            0..6,
            HighlightStyle {
                font_weight: Some(gpui::FontWeight::BOLD),
                ..Default::default()
            },
        )];
        merge_code_highlights(
            &mut highlights,
            &[CodeSpan {
                range: 0..6,
                fg: blue(),
            }],
        );
        assert_eq!(highlights.len(), 1);
        assert_eq!(highlights[0].1.color, Some(blue()));
        assert_eq!(highlights[0].1.font_weight, Some(gpui::FontWeight::BOLD));
    }

    #[gpui::test]
    async fn measurement_answer_is_independent_of_prior_constraint(cx: &mut TestAppContext) {
        let window = cx.add_window(|_, _| Empty);
        let mut visual = VisualTestContext::from_window(window.into(), cx);
        visual.update(|window, _cx| {
            let text: SharedString = "alpha beta gamma 你好世界 table-cell-content Cargo.toml"
                .repeat(4)
                .into();
            let text_style = window.text_style();
            let font_size = text_style.font_size.to_pixels(window.rem_size());
            let line_height = window.pixel_snap(
                text_style
                    .line_height
                    .to_pixels(font_size.into(), window.rem_size()),
            );
            let runs = vec![text_style.to_run(text.len())];
            let constraints = [
                AvailableSpace::MinContent,
                AvailableSpace::MaxContent,
                AvailableSpace::Definite(px(80.)),
                AvailableSpace::Definite(px(320.)),
            ];

            for second in constraints {
                let fresh = measure_shape(
                    &Rc::new(RefCell::new(ShapeCache::default())),
                    None,
                    second,
                    &text,
                    &runs,
                    &text_style,
                    font_size,
                    line_height,
                    window,
                );
                for first in constraints {
                    let cache = Rc::new(RefCell::new(ShapeCache::default()));
                    measure_shape(
                        &cache,
                        None,
                        first,
                        &text,
                        &runs,
                        &text_style,
                        font_size,
                        line_height,
                        window,
                    );
                    let with_history = measure_shape(
                        &cache,
                        None,
                        second,
                        &text,
                        &runs,
                        &text_style,
                        font_size,
                        line_height,
                        window,
                    );
                    assert_eq!(
                        with_history, fresh,
                        "measuring {second:?} after {first:?} changed the answer"
                    );
                }
            }
        });
    }

    #[gpui::test]
    async fn zero_and_one_pixel_probes_do_not_explode_height(cx: &mut TestAppContext) {
        let window = cx.add_window(|_, _| Empty);
        let mut visual = VisualTestContext::from_window(window.into(), cx);
        let text = (0..100)
            .map(|ix| format!("constraint-probe-{ix}"))
            .collect::<Vec<_>>()
            .join(" ");
        for width in [px(0.), px(1.)] {
            LAST_MEASURED_SIZE.with(|slot| slot.replace(None));
            let _ = visual.draw(
                point(px(0.), px(0.)),
                size(AvailableSpace::Definite(width), AvailableSpace::MinContent),
                |_window, _cx| RichText::new(text.clone(), 0, DocSelection::new()),
            );
            let measured = LAST_MEASURED_SIZE
                .with(|slot| *slot.borrow())
                .expect("RichText was measured");
            assert!(
                measured.height < px(100.),
                "narrow intrinsic probe produced an explosive height: {measured:?}"
            );
        }
    }

    #[gpui::test]
    async fn definite_width_wraps_and_reports_full_height(cx: &mut TestAppContext) {
        let window = cx.add_window(|_, _| Empty);
        let mut visual = VisualTestContext::from_window(window.into(), cx);
        LAST_MEASURED_SIZE.with(|slot| slot.replace(None));
        let _ = visual.draw(
            point(px(0.), px(0.)),
            size(
                AvailableSpace::Definite(px(480.)),
                AvailableSpace::MinContent,
            ),
            |_window, _cx| RichText::new("wrapped words ".repeat(100), 0, DocSelection::new()),
        );
        let measured = LAST_MEASURED_SIZE
            .with(|slot| *slot.borrow())
            .expect("RichText was measured");
        assert!(measured.width <= px(480.));
        assert!(measured.height > px(100.));
    }

    #[gpui::test]
    async fn painted_shape_drives_selection_and_link_geometry(cx: &mut TestAppContext) {
        let window = cx.add_window(|_, _| Empty);
        let mut visual = VisualTestContext::from_window(window.into(), cx);
        let selection = DocSelection::new();
        let selection_for_draw = selection.clone();
        let _ = visual.draw(
            point(px(12.), px(20.)),
            size(
                AvailableSpace::Definite(px(480.)),
                AvailableSpace::MinContent,
            ),
            |_window, _cx| {
                RichText::new("link tail", 0, selection_for_draw).link_spans(vec![LinkSpan {
                    range: 0..4,
                    url: "https://example.com".into(),
                    kind: crate::markdown::ast::LinkKind::Url,
                }])
            },
        );

        let hit = selection
            .hit(point(px(14.), px(22.)))
            .expect("paint registered shaped text geometry");
        assert!(hit < 4, "point over link resolved outside its span: {hit}");
        assert_eq!(
            selection.link_at(hit).expect("link at hit").url,
            "https://example.com"
        );
    }
}
