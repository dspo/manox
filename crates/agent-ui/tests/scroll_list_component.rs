#![cfg(feature = "message-list-gpui-component")]

use agent_ui::views::{
    centered,
    vlist::{FollowMode, VListState, vlist},
};
use gpui::{
    AppContext as _, Context, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    Styled as _, TestAppContext, VisualTestContext, Window, div, px, size,
};
use gpui_component::{ActiveTheme as _, v_flex};
use manox_components::markdown::Markdown;

struct PersistentMarkdownRow {
    id: usize,
    markdown: gpui::Entity<Markdown>,
}

impl Render for PersistentMarkdownRow {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let id = self.id;
        centered(self.markdown.clone())
            .debug_selector(move || format!("component-message-content-{id}"))
    }
}

struct ComponentListProbe {
    state: VListState,
    rows: Vec<gpui::Entity<PersistentMarkdownRow>>,
}

impl Render for ComponentListProbe {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state.clone();
        let view = cx.entity().clone();
        v_flex()
            .size_full()
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .debug_selector(|| "component-list-viewport".into())
                    .child(
                        vlist(view, state, |this, ix, _window, _cx| {
                            v_flex()
                                .w_full()
                                .pt_1()
                                .pb_4()
                                .min_w_0()
                                .flex_shrink_0()
                                .debug_selector(move || format!("component-message-row-{ix}"))
                                .child(this.rows[ix].clone())
                                .into_any_element()
                        })
                        .size_full(),
                    ),
            )
            .child(
                div()
                    .h(px(80.))
                    .w_full()
                    .flex_shrink_0()
                    .debug_selector(|| "component-list-footer".into()),
            )
    }
}

fn long_plan() -> String {
    (0..100)
        .map(|ix| {
            format!("## Section {ix}\n\n- 这是一段需要在确定宽度下换行的长 plan 内容 {ix}\n\n")
        })
        .collect()
}

fn open_probe(
    cx: &mut TestAppContext,
    first_source: String,
) -> (VisualTestContext, VListState, gpui::Entity<Markdown>) {
    cx.update(gpui_component::init);
    let first_md = cx.new(|cx| {
        Markdown::new("component-plan", first_source)
            .theme(cx.theme())
            .streaming(true)
    });
    let tail_md = cx.new(|cx| Markdown::new("component-tail", "tail message").theme(cx.theme()));
    let first = cx.new(|_| PersistentMarkdownRow {
        id: 0,
        markdown: first_md.clone(),
    });
    let tail = cx.new(|_| PersistentMarkdownRow {
        id: 1,
        markdown: tail_md,
    });
    let state = VListState::new(2);
    state.set_follow_mode(FollowMode::Tail);
    let window = cx.open_window(size(px(760.), px(600.)), {
        let state = state.clone();
        move |_, _| ComponentListProbe {
            state,
            rows: vec![first, tail],
        }
    });
    cx.run_until_parked();
    let mut visual = VisualTestContext::from_window(window.into(), cx);
    visual.update(|window, cx| window.draw(cx).clear());
    visual.run_until_parked();
    visual.update(|window, cx| window.draw(cx).clear());
    (visual, state, first_md)
}

fn assert_rows_end_above_footer(visual: &mut VisualTestContext) {
    let first = visual
        .debug_bounds("component-message-content-0")
        .expect("long message content");
    let tail = visual
        .debug_bounds("component-message-content-1")
        .expect("tail message content");
    let footer = visual
        .debug_bounds("component-list-footer")
        .expect("footer");
    assert!(
        first.bottom() <= tail.top(),
        "component list content overlaps: first={first:?}, tail={tail:?}"
    );
    assert!(
        tail.bottom() <= footer.top(),
        "tail is hidden behind the footer: tail={tail:?}, footer={footer:?}"
    );
}

#[gpui::test]
async fn component_list_settles_long_rows_and_keeps_tail_above_footer(cx: &mut TestAppContext) {
    let (mut visual, state, _first_md) = open_probe(cx, long_plan());
    let viewport = visual
        .debug_bounds("component-list-viewport")
        .expect("viewport");
    let footer = visual
        .debug_bounds("component-list-footer")
        .expect("footer");
    assert_eq!(viewport.bottom(), footer.top());
    assert!(state.total_height() > viewport.size.height);
    let (top, max, _total) = state.scroll_geometry();
    assert_eq!(top, max, "tail-follow must settle at the real content end");
    assert_rows_end_above_footer(&mut visual);
}

#[gpui::test]
async fn component_list_remeasures_markdown_child_growth_without_explicit_invalidation(
    cx: &mut TestAppContext,
) {
    let (mut visual, state, first_md) = open_probe(cx, "# Plan\n\nInitial".into());
    let initial_total = state.total_height();
    first_md.update(&mut visual.cx, |markdown, cx| {
        markdown.replace(long_plan(), cx)
    });
    visual.run_until_parked();
    visual.update(|window, cx| window.draw(cx).clear());
    visual.run_until_parked();
    visual.update(|window, cx| window.draw(cx).clear());

    assert!(
        state.total_height() > initial_total + px(600.),
        "child growth did not reach the component item-size snapshot"
    );
    assert_rows_end_above_footer(&mut visual);
}
