//! Pins the blank-band invariant: a message row may exceed its markdown root
//! only by the fixed message chrome.
//!
//! Two conditions must hold together to expose the bug, which is why every
//! earlier test missed it:
//! 1. the viewport is **wider** than `CONTENT_MAX_W`, so the centering wrapper
//!    has horizontal slack, and
//! 2. the body contains a block whose height depends strongly on its width — a
//!    table with long cells is the extreme case.
//!
//! Under those conditions a row-flex centering wrapper derives its own height
//! from the cross size the capped child reports at a *probe* width rather than
//! the resolved one, so wrapped text reports a far taller height than it paints
//! and the surplus stays in the row as blank space. Neither the per-row content
//! mask (which only clips rows painting *more* than they report) nor any list
//! implementation can observe this.
//!
//! `table_row_in_list_matches_its_standalone_layout` additionally proves the
//! surplus is not the list's measurement space: the same content measures
//! identically inside `ChatList` and as a plain child.

use agent_ui::{Workspace, conversation::ConvItem, views::message::MessageItem};
use gpui::{
    AppContext as _, Context, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    Styled as _, TestAppContext, VisualTestContext, Window, px, size,
};
use gpui_component::{ActiveTheme as _, h_flex, v_flex};
use manox_components::chat_list::{ChatList, ChatListState, RowKey};

/// The body shape that exposes the bug: a heading, a three-column table whose
/// cells run to a few hundred characters, then bullet lists and a paragraph.
const TABLE_BODY: &str = include_str!("fixtures/table_row.md");

const WIDTH: f32 = 1240.;

fn table_item(cx: &mut TestAppContext, ix: usize) -> gpui::Entity<MessageItem> {
    let weak = gpui::WeakEntity::<Workspace>::new_invalid();
    let kind = ConvItem::Assistant {
        text: TABLE_BODY.to_string(),
        streaming: false,
        token_usage: None,
        activity_summary: None,
    };
    cx.new(|_| MessageItem::new(kind, "deepseek-v4-flash".into(), ix, weak.clone()))
}

/// Hosts the same content twice at the same width: once as a `ChatList` row,
/// once as a plain child of an auto-height column.
struct Probe {
    in_list: gpui::Entity<MessageItem>,
    standalone: gpui::Entity<MessageItem>,
    state: ChatListState,
}

impl Render for Probe {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mono_family = cx.theme().mono_font_family.clone();
        let row = self.in_list.clone();
        let keys = vec![RowKey::from_entity_id(row.entity_id())];
        let list = ChatList::new(self.state.clone(), keys, move |_ix, _window, _cx| {
            v_flex()
                .w_full()
                .pt_1()
                .pb_4()
                .flex_shrink_0()
                .min_w_0()
                .debug_selector(|| "list-row".into())
                .child(row.clone())
                .into_any_element()
        })
        .w_full()
        .h_full()
        .min_h_0()
        .min_w_0();

        v_flex()
            .w(px(WIDTH))
            .font_family(mono_family)
            .font_weight(gpui::FontWeight::LIGHT)
            // Tall enough that the single row never needs scrolling, so its
            // slot height is the height it reported, unclamped.
            .child(
                h_flex()
                    .w_full()
                    .h(px(3200.))
                    .min_h_0()
                    .min_w_0()
                    .overflow_hidden()
                    .child(v_flex().flex_1().h_full().min_h_0().min_w_0().child(list)),
            )
            // Same wrapper, no list: this is what a plain auto-height parent
            // resolves the very same content to.
            .child(
                v_flex()
                    .w_full()
                    .pt_1()
                    .pb_4()
                    .flex_shrink_0()
                    .min_w_0()
                    .debug_selector(|| "plain-row".into())
                    .child(self.standalone.clone()),
            )
    }
}

fn draw(cx: &mut TestAppContext) -> VisualTestContext {
    cx.update(gpui_component::init);
    let in_list = table_item(cx, 0);
    let standalone = table_item(cx, 1);
    let state = ChatListState::bottom_aligned();
    state.follow_tail();
    let window = cx.open_window(size(px(WIDTH), px(7000.)), move |_, _| Probe {
        in_list,
        standalone,
        state,
    });
    cx.run_until_parked();
    let mut visual = VisualTestContext::from_window(window.into(), cx);
    visual.update(|window, cx| window.draw(cx).clear());
    visual
}

#[gpui::test]
async fn table_row_in_list_matches_its_standalone_layout(cx: &mut TestAppContext) {
    let mut visual = draw(cx);
    let listed = visual.debug_bounds("list-row").expect("list row");
    let plain = visual.debug_bounds("plain-row").expect("plain row");
    assert!(
        (listed.size.height - plain.size.height).abs() <= px(1.),
        "the same content measures differently inside the list than as a plain \
         child: list={:?} plain={:?}",
        listed.size.height,
        plain.size.height
    );
}

/// The message chrome around the markdown root (header row, `centered()`
/// wrapper, row padding) is a fixed ~50px. Anything beyond that is surplus
/// height with nothing painted in it — the blank band.
#[gpui::test]
async fn row_height_exceeds_markdown_root_only_by_the_chrome(cx: &mut TestAppContext) {
    let mut visual = draw(cx);
    let plain = visual.debug_bounds("plain-row").expect("plain row");
    let root = visual.debug_bounds("md-root-1").expect("markdown root");
    let chrome = plain.size.height - root.size.height;
    assert!(
        chrome <= px(80.),
        "row is {chrome:?} taller than its markdown root — surplus with nothing \
         to paint in it: row={:?} root={:?}",
        plain.size.height,
        root.size.height
    );
}
