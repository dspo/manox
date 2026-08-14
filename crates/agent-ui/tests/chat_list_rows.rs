//! `ChatList` under **real** message rows.
//!
//! The synthetic-height suite in `chat_list.rs` feeds row heights in as test
//! input, so by construction it cannot observe a row that mis-reports its own
//! height. These cases drive the production `MessageItem` / `Markdown` /
//! `TerminalPanel` tree instead, which is where a measure/paint mismatch
//! actually lives: a row whose slot is shorter than its content is silently
//! clipped by the per-row content mask, and an under-measured total makes
//! bottom alignment leave a blank band above the first row.

use agent::ToolCallStatus;
use agent_ui::{
    Workspace,
    conversation::{
        ActivityEntry, ConvItem, ThinkingContainer, ToolCallItem, UserMessageDisplayState,
    },
    views::message::MessageItem,
};
use gpui::{
    AppContext as _, Bounds, Context, InteractiveElement as _, IntoElement, ParentElement as _,
    Pixels, Render, Styled as _, TestAppContext, VisualTestContext, Window, div, px, size,
};
use gpui_component::{ActiveTheme as _, h_flex, v_flex};
use manox_components::chat_list::{ChatList, ChatListState, RowKey};

/// A conversation mixing every row shape the production column renders: a long
/// wrapped assistant body, a short user bubble, a thinking container holding a
/// reasoning entry plus a tool card, and a second long assistant body.
fn production_rows(cx: &mut TestAppContext) -> Vec<gpui::Entity<MessageItem>> {
    let weak = gpui::WeakEntity::<Workspace>::new_invalid();
    let mut activity = ThinkingContainer::new();
    activity.accepting_entries = false;
    activity.streaming = false;
    activity.collapsed = false;
    activity.user_toggled = true;
    activity.entries = vec![
        ActivityEntry::Reasoning {
            text: "我会先检查 tools_param 在 completions 和 responses 两条 wire 的实现。".repeat(8),
            streaming: false,
            collapsed: false,
            user_toggled: true,
            markdown: None,
        },
        ActivityEntry::Tool(ToolCallItem {
            id: "grep-properties".into(),
            name: "Bash".into(),
            title: "$ grep -rn properties ~/projects/github/pi/packages/*/src/*.ts".into(),
            status: ToolCallStatus::Success,
            output: (0..30)
                .map(|ix| format!("packages/protocol/src/schemas.ts:{ix}: Type.Object(properties)"))
                .collect::<Vec<_>>()
                .join("\n"),
            is_error: false,
            input: serde_json::json!({"command": "grep -rn properties"}),
            streaming: false,
            collapsed: false,
            user_toggled: true,
            panel: None,
        }),
    ];
    vec![
        ConvItem::Assistant {
            text: "所以这不是模型能力问题，而是 responses wire 独享的端点校验差异。".repeat(18),
            streaming: false,
            token_usage: None,
            activity_summary: None,
        },
        ConvItem::User {
            text: "需要修本项目吗？".into(),
            images: Vec::new(),
            meta: None,
            display_state: UserMessageDisplayState::Normal,
        },
        ConvItem::Thinking(activity),
        ConvItem::Assistant {
            text: "需要。这是 manox 自己的 bug，上游修不到也不该修。".repeat(20),
            streaming: false,
            token_usage: None,
            activity_summary: None,
        },
    ]
    .into_iter()
    .enumerate()
    .map(|(ix, kind)| {
        let item = cx.new(|_| MessageItem::new(kind, "deepseek-v4-flash".into(), ix, weak.clone()));
        item.update(cx, |item, cx| {
            item.rebuild_activity_reasoning(cx);
            item.rebuild_tool_panels(None, cx);
        });
        item
    })
    .collect()
}

/// Mirrors the production nesting: `h_flex(overflow_hidden) > v_flex(font) >
/// ChatList`, with the same per-row wrapper the workspace row factory builds.
struct RowsProbe {
    state: ChatListState,
    rows: Vec<gpui::Entity<MessageItem>>,
}

impl Render for RowsProbe {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.rows.clone();
        let keys: Vec<RowKey> = rows
            .iter()
            .map(|r| RowKey::from_entity_id(r.entity_id()))
            .collect();
        let mono_family = cx.theme().mono_font_family.clone();
        let list = ChatList::new(self.state.clone(), keys, move |ix, _window, _cx| {
            v_flex()
                .w_full()
                .pt_1()
                .pb_4()
                .flex_shrink_0()
                .min_w_0()
                .debug_selector(move || format!("prod-row-{ix}"))
                .child(rows[ix].clone())
                .into_any_element()
        })
        .w_full()
        .h_full()
        .min_h_0()
        .min_w_0();

        h_flex()
            .size_full()
            .min_h_0()
            .min_w_0()
            .overflow_hidden()
            .child(
                v_flex()
                    .flex_1()
                    .h_full()
                    .min_h_0()
                    .min_w_0()
                    .font_family(mono_family)
                    .font_weight(gpui::FontWeight::LIGHT)
                    .child(list),
            )
    }
}

struct Draw {
    visual: VisualTestContext,
    state: ChatListState,
    viewport: Bounds<Pixels>,
}

fn draw_rows(cx: &mut TestAppContext, width: f32, height: f32) -> Draw {
    cx.update(gpui_component::init);
    let rows = production_rows(cx);
    let state = ChatListState::bottom_aligned();
    state.follow_tail();
    let window = cx.open_window(size(px(width), px(height)), {
        let state = state.clone();
        move |_, _| RowsProbe { state, rows }
    });
    cx.run_until_parked();
    let mut visual = VisualTestContext::from_window(window.into(), cx);
    visual.update(|window, cx| window.draw(cx).clear());
    let viewport = Bounds {
        origin: gpui::point(px(0.), px(0.)),
        size: size(px(width), px(height)),
    };
    Draw {
        visual,
        state,
        viewport,
    }
}

/// `debug_bounds` takes a `'static` selector, so the row selectors are spelled
/// out rather than formatted.
const ROW_SELECTORS: [&str; 4] = ["prod-row-0", "prod-row-1", "prod-row-2", "prod-row-3"];

fn row_bounds(visual: &mut VisualTestContext, n: usize) -> Vec<Bounds<Pixels>> {
    ROW_SELECTORS
        .iter()
        .take(n)
        .filter_map(|selector| visual.debug_bounds(selector))
        .collect()
}

/// Rows must tile the list without overlapping, and the painted activity tree
/// of the thinking row must stay inside that row's slot — if the slot is
/// shorter than the tree, the per-row mask clips real content.
#[gpui::test]
async fn production_rows_tile_without_overlap_and_contain_their_content(cx: &mut TestAppContext) {
    let mut draw = draw_rows(cx, 760., 4000.);
    for _ in 0..2 {
        draw.visual.update(|window, cx| window.draw(cx).clear());
        let bounds = row_bounds(&mut draw.visual, 4);
        assert_eq!(bounds.len(), 4, "every production row should be placed");
        for pair in bounds.windows(2) {
            assert!(
                pair[0].bottom() <= pair[1].top() + px(0.5),
                "production rows overlap: {:?} then {:?}",
                pair[0],
                pair[1]
            );
        }
        let row = draw
            .visual
            .debug_bounds("prod-row-2")
            .expect("thinking row");
        let tree = draw
            .visual
            .debug_bounds("message-overflow-activity-tree-2")
            .expect("activity tree");
        assert!(
            tree.top() >= row.top() - px(0.5) && tree.bottom() <= row.bottom() + px(0.5),
            "painted activity escaped its row slot (the per-row mask is clipping \
             real content): row={row:?}, tree={tree:?}"
        );
        // The mask only defends against a row reporting *less* than it paints.
        // A row reporting *more* is what turns into a blank band, and nothing
        // in the list can detect it — so pin it here: the slot may exceed its
        // content only by the wrapper's own padding (`pt_1` + `pb_4` = 20px).
        let slack = row.size.height - tree.size.height;
        assert!(
            slack <= px(24.),
            "row slot is taller than the content it paints by {slack:?} — this is \
             the blank-band direction: row={row:?}, tree={tree:?}"
        );
    }
}

/// Content shorter than the viewport: bottom alignment parks it against the
/// viewport bottom and the list must report itself at the bottom, so the
/// jump-to-latest button stays hidden.
#[gpui::test]
async fn short_content_parks_at_the_bottom_and_reports_at_bottom(cx: &mut TestAppContext) {
    let mut draw = draw_rows(cx, 760., 4000.);
    let bounds = row_bounds(&mut draw.visual, 4);
    let last = *bounds.last().expect("last row");
    assert!(
        (last.bottom() - draw.viewport.bottom()).abs() <= px(1.),
        "bottom-aligned content should end on the viewport bottom: \
         last={last:?}, viewport={:?}",
        draw.viewport
    );
    assert_eq!(
        draw.state.is_at_bottom(),
        Some(true),
        "content that fits the viewport is at the bottom"
    );
}

/// Content taller than the viewport while following the tail: the viewport must
/// be filled from the live end upward with no blank band above the first placed
/// row. A blank band means the frame settled above the content top.
#[gpui::test]
async fn overflowing_content_leaves_no_blank_band_above_the_first_row(cx: &mut TestAppContext) {
    let mut draw = draw_rows(cx, 760., 600.);
    let bounds = row_bounds(&mut draw.visual, 4);
    let first = *bounds.first().expect("at least one placed row");
    assert!(
        first.top() <= draw.viewport.top() + px(1.),
        "blank band above the first placed row while content overflows: \
         first={first:?}, viewport={:?}",
        draw.viewport
    );
    let last = *bounds.last().expect("last row");
    assert!(
        last.bottom() >= draw.viewport.bottom() - px(1.),
        "tail-following viewport should be filled down to its bottom: \
         last={last:?}, viewport={:?}",
        draw.viewport
    );
}

/// A narrow width caches nothing, so restoring the width must restore the
/// layout in a single frame with no residual blank range.
#[gpui::test]
async fn narrow_width_then_restore_leaves_no_blank_range(cx: &mut TestAppContext) {
    let mut draw = draw_rows(cx, 320., 600.);
    draw.visual.simulate_resize(size(px(760.), px(600.)));
    draw.visual.update(|window, cx| window.draw(cx).clear());
    let viewport = Bounds {
        origin: gpui::point(px(0.), px(0.)),
        size: size(px(760.), px(600.)),
    };
    let bounds = row_bounds(&mut draw.visual, 4);
    let first = *bounds.first().expect("at least one placed row");
    assert!(
        first.top() <= viewport.top() + px(1.),
        "blank band survived a width restore: first={first:?}, viewport={viewport:?}"
    );
}

/// The list must not paint rows outside its own bounds — a row whose slot
/// extends past the viewport is legitimately partially clipped, but no row's
/// slot may start below the viewport bottom.
#[gpui::test]
async fn no_row_slot_starts_below_the_viewport(cx: &mut TestAppContext) {
    let mut draw = draw_rows(cx, 760., 600.);
    for b in row_bounds(&mut draw.visual, 4) {
        assert!(
            b.top() < draw.viewport.bottom() + px(1.),
            "row slot starts below the viewport: {b:?}, viewport={:?}",
            draw.viewport
        );
    }
    // Keep `div` in use for the import set the probe shares with chat_list.rs.
    let _ = div();
}
