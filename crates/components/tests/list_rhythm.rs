//! List blocks must occupy the same vertical footprint as equivalent body
//! paragraphs, and `body_size`/heading tiers must render at their configured
//! sizes. The first guard catches the marker-wrap regression (a too-narrow
//! marker column wrapped "• " / "N. " onto an invisible extra line, inflating
//! every item by a full line box). The second mounts the real `Root` (which
//! pins `rem` to `theme.font_size`, 14px in the app) and pins the rendered
//! line boxes: body 13px, H1 at 1rem (14px), H2+ back at body size.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    AnyElement, App, AppContext, Bounds, Element, GlobalElementId, InspectorElementId, IntoElement,
    LayoutId, ParentElement, Pixels, Render, Style, Styled, Window, div, px,
};
use gpui_component::{Root, Theme};
use manox_components::markdown::{HeadingMode, Markdown};

struct Measure {
    child: AnyElement,
    out: Rc<Cell<Pixels>>,
}

impl IntoElement for Measure {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for Measure {
    type RequestLayoutState = ();
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
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let child_id = self.child.request_layout(window, cx);
        (window.request_layout(Style::default(), [child_id], cx), ())
    }
    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _state: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.out.set(bounds.size.height);
        self.child.prepaint(window, cx);
    }
    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _state: &mut (),
        _prepaint: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.paint(window, cx);
    }
}

struct Host {
    source: String,
    body: Option<Pixels>,
    out: Rc<Cell<Pixels>>,
}

impl Render for Host {
    fn render(&mut self, _window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let mut md = Markdown::new("m", self.source.clone())
            .theme(&Theme::default())
            .heading_mode(HeadingMode::Uniform);
        if let Some(b) = self.body {
            md = md.body_size(b);
        }
        let md = cx.new(|_cx| md);
        div().w(px(600.)).text_base().child(Measure {
            child: md.into_any_element(),
            out: self.out.clone(),
        })
    }
}

fn measure_raw(cx: &mut gpui::TestAppContext, source: &str, body: Option<Pixels>) -> f32 {
    let out = Rc::new(Cell::new(px(0.)));
    let src = source.to_string();
    let o2 = out.clone();
    let (_view, cx) = cx.add_window_view(move |_window, _cx| Host {
        source: src,
        body,
        out: o2,
    });
    cx.run_until_parked();
    f32::from(out.get())
}

fn measure(cx: &mut gpui::TestAppContext, source: &str) -> f32 {
    measure_raw(cx, source, None)
}

#[gpui::test]
fn list_vertical_footprint_matches_body_paragraphs(cx: &mut gpui::TestAppContext) {
    let lines = [
        "alpha bravo charlie",
        "delta echo foxtrot",
        "golf hotel india",
        "juliet kilo lima",
        "mike november oscar",
        "papa quebec romeo",
        "sierra tango uniform",
        "victor whiskey xray",
        "yankee zulu one",
        "two three four",
        "five six seven",
        "eight nine ten",
    ];
    let paras = lines.join("\n\n");
    let tight = lines
        .iter()
        .map(|l| format!("- {l}"))
        .collect::<Vec<_>>()
        .join("\n");
    let ordered = lines
        .iter()
        .map(|l| format!("1. {l}"))
        .collect::<Vec<_>>()
        .join("\n");

    let one_par = measure(cx, lines[0]);
    let one_list = measure(cx, &format!("- {}", lines[0]));
    let twelve_par = measure(cx, &paras);
    let twelve_tight = measure(cx, &tight);
    let twelve_ordered = measure(cx, &ordered);

    assert!(
        (one_list - one_par).abs() <= 1.0,
        "single-item list {one_list}px must match single paragraph {one_par}px"
    );
    assert!(
        (twelve_tight - twelve_par).abs() <= 1.0,
        "12-item list {twelve_tight}px must match 12 paragraphs {twelve_par}px"
    );
    assert!(
        (twelve_ordered - twelve_par).abs() <= 1.0,
        "12-item ordered list {twelve_ordered}px must match 12 paragraphs {twelve_par}px"
    );
}

/// Mirrors the app: `Root` pins `rem` to `theme.font_size` (14px), so `1rem`
/// headings render at 14 while a `body_size(px(13.))` document renders at 13.
/// Line box = phi (≈1.618) × font size, pixel-snapped: 13px → 21, 14px → 23.
/// The root col's Sentinel child plus its gap_2 add a constant 7px (0.5rem @
/// rem 14) to every single-block document, so measured heights are line + 7.
#[gpui::test]
fn body_size_and_heading_tiers_render_at_configured_sizes(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    cx.update(|cx| {
        gpui_component::Theme::global_mut(cx).font_size = px(14.);
    });

    // Wrap the host in the real Root so set_rem_size(14) applies, like the app.
    let measure = |cx: &mut gpui::TestAppContext, source: &str, body: Option<Pixels>| {
        let out = Rc::new(Cell::new(px(0.)));
        let src = source.to_string();
        let o2 = out.clone();
        let (_view, cx) = cx.add_window_view(move |window, cx| {
            let host = cx.new(|_cx| Host {
                source: src,
                body,
                out: o2,
            });
            Root::new(host, window, cx)
        });
        cx.run_until_parked();
        f32::from(out.get())
    };

    let body_default = measure(cx, "alpha bravo charlie", None);
    let body_13 = measure(cx, "alpha bravo charlie", Some(px(13.)));
    let h1_at_13 = measure(cx, "# alpha", Some(px(13.)));
    let h4_at_13 = measure(cx, "#### alpha", Some(px(13.)));

    assert!(
        (body_13 - 28.0).abs() <= 1.0,
        "body_size(13px) single line ≈21px + 7px sentinel gap, got {body_13}"
    );
    assert!(
        (body_default - body_13 - 1.6).abs() <= 1.0,
        "1rem body must exceed 13px body by one phi step (≈1.6px), got {body_default} vs {body_13}"
    );
    // H1 carries mb_2 (space_after), hence one extra 7px over body_default.
    assert!(
        (h1_at_13 - body_default - 7.0).abs() <= 1.0,
        "Uniform H1 line must equal the 1rem body line (+mb_2), got {h1_at_13} vs {body_default}"
    );
    // H4+ has no space_after: pure line-box comparison against the 13px body.
    assert!(
        (h4_at_13 - body_13).abs() <= 0.5,
        "Uniform H4 must follow the 13px body, got {h4_at_13} vs {body_13}"
    );
}
