//! List blocks must occupy the same vertical footprint as equivalent body
//! paragraphs. A regression here (marker column too narrow for its mono
//! marker text) wrapped "• " / "N. " onto an invisible extra line, inflating
//! every list item by a full line box and blowing up item spacing.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    AnyElement, App, AppContext, Bounds, Element, GlobalElementId, InspectorElementId, IntoElement,
    LayoutId, ParentElement, Pixels, Render, Style, Styled, Window, div, px,
};
use gpui_component::Theme;
use manox_components::markdown::Markdown;

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
    md: gpui::Entity<Markdown>,
    out: Rc<Cell<Pixels>>,
}

impl Render for Host {
    fn render(&mut self, _window: &mut Window, _cx: &mut gpui::Context<Self>) -> impl IntoElement {
        div().w(px(600.)).text_base().child(Measure {
            child: self.md.clone().into_any_element(),
            out: self.out.clone(),
        })
    }
}

fn measure(cx: &mut gpui::TestAppContext, source: &str) -> f32 {
    let out = Rc::new(Cell::new(px(0.)));
    let src = source.to_string();
    let out2 = out.clone();
    let (_view, cx) = cx.add_window_view(move |_window, cx| {
        let md = cx.new(|_cx| Markdown::new("m", src).theme(&Theme::default()));
        Host { md, out: out2 }
    });
    cx.run_until_parked();
    f32::from(out.get())
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
