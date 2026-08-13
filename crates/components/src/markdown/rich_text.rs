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

#[derive(Default)]
struct ShapedText {
    lines: Rc<Vec<WrappedLine>>,
    size: Size<Pixels>,
    wrap_width: Option<Pixels>,
    line_height: Pixels,
    bounds: Option<Bounds<Pixels>>,
}

#[cfg(test)]
thread_local! {
    static LAST_MEASURED_SIZE: RefCell<Option<Size<Pixels>>> = const { RefCell::new(None) };
}

pub struct RichTextState {
    shaped: Rc<RefCell<ShapedText>>,
    text: SharedString,
    runs: Vec<TextRun>,
    font_size: Pixels,
    line_height: Pixels,
    text_style: TextStyle,
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
        let shaped = Rc::new(RefCell::new(ShapedText::default()));
        let measured = shaped.clone();
        let layout_id = window.request_measured_layout(
            Style::default(),
            move |known, available, window, _cx| {
                let wrap_width = useful_wrap_width(
                    known.width,
                    available.width,
                    font_size,
                    measure_style.white_space,
                );
                let next = shape(
                    measure_text.clone(),
                    &measure_runs,
                    font_size,
                    line_height,
                    wrap_width,
                    measure_style.line_clamp,
                    window,
                );
                let size = next.size;
                #[cfg(test)]
                LAST_MEASURED_SIZE.with(|slot| slot.replace(Some(size)));
                *measured.borrow_mut() = next;
                size
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
        let current_width = state.shaped.borrow().wrap_width;
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
                *state.shaped.borrow_mut() = replacement;
            }
        }
        state.shaped.borrow_mut().bounds = Some(bounds);
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
        let shaped = state.shaped.borrow();
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
