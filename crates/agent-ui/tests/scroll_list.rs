//! Probe the message list's native `gpui::list` semantics under the exact
//! nesting the production message column uses: bottom alignment gives chat-log
//! layout — short histories sit at the bottom of the viewport, long ones
//! scroll, and `FollowMode::Tail` re-pins to the end each layout while
//! following. A regression in the list's scroll

use std::{cell::RefCell, rc::Rc};

use agent::ToolCallStatus;
use agent_ui::{
    Workspace,
    conversation::{
        ActivityEntry, ConvItem, ThinkingContainer, ToolCallItem, UserMessageDisplayState,
    },
    views::{MessageListWidthInvalidator, message::MessageItem},
};
use gpui::{
    AnyWindowHandle, AppContext as _, Context, FollowMode, InteractiveElement as _, IntoElement,
    ListAlignment, ListState, Modifiers, MouseButton, ParentElement as _, Pixels, Render,
    Styled as _, TestAppContext, VisualTestContext, Window, WindowHandle, div, list, px, size,
};
use gpui_component::{ActiveTheme as _, ElementExt as _};
use manox_components::markdown::{Markdown, PanelKind, TerminalPanel};

struct ListProbe {
    state: ListState,
    body: Rc<RefCell<Vec<Pixels>>>,
    viewport_h: Pixels,
}

impl Render for ListProbe {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let body = self.body.clone();
        let state = self.state.clone();
        div()
            .id("row")
            .w(px(100.))
            .h(self.viewport_h)
            .flex()
            .flex_row()
            .items_center()
            .child(
                div()
                    .id("wrap")
                    .flex_1()
                    .h_full()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(
                        list(state, move |ix, _window, _cx| {
                            let height = body.borrow().get(ix).copied().unwrap_or(px(0.));
                            div()
                                .id(("vc", ix))
                                .w(px(100.))
                                .h(height)
                                .flex_shrink_0()
                                .into_any_element()
                        })
                        .w_full()
                        .h_full()
                        .min_h_0(),
                    ),
            )
    }
}

fn draw_list(
    cx: &mut TestAppContext,
    body: Vec<Pixels>,
    viewport_h: Pixels,
) -> (WindowHandle<ListProbe>, Rc<RefCell<Vec<Pixels>>>, ListState) {
    let state = ListState::new(body.len(), ListAlignment::Bottom, px(2048.));
    let build = state.clone();
    let body = Rc::new(RefCell::new(body));
    let window = cx.add_window({
        let body = body.clone();
        let build = build.clone();
        move |_, _| ListProbe {
            state: build,
            body: body.clone(),
            viewport_h,
        }
    });
    redraw(cx, window.into());
    (window, body, state)
}

fn redraw(cx: &mut TestAppContext, any: AnyWindowHandle) {
    cx.run_until_parked();
    cx.update_window(any, |_, window, cx| {
        window.draw(cx).clear();
    })
    .unwrap();
}

/// Short content (fits the viewport): nothing to scroll, every row measured,
/// and the logical scroll top stays at the content end while bottom alignment
/// places the rows at the viewport bottom (chat-log semantics, composer below
/// the last message).
#[gpui::test]
async fn list_bottom_anchors_short_content_in_h_flex_row(cx: &mut TestAppContext) {
    let (_window, _body, state) = draw_list(cx, vec![px(40.), px(40.)], px(100.));
    let top = state.logical_scroll_top();
    assert_eq!(
        top.item_ix, 2,
        "Bottom alignment anchors at the content end"
    );
    assert_eq!(top.offset_in_item, px(0.));
    assert_eq!(
        state.is_scrolled_to_end(),
        None,
        "fitting content is not scrollable"
    );
}

/// Long content with `FollowMode::Tail`: the scroll top pins to the content
/// end on each layout while following.
#[gpui::test]
async fn list_tail_follow_pins_end(cx: &mut TestAppContext) {
    let (window, _body, state) = draw_list(cx, vec![px(40.); 4], px(100.));
    state.set_follow_mode(FollowMode::Tail);
    // Redraw so the list consumes the follow state and re-anchors at the end.
    redraw(cx, window.into());
    assert!(
        state.is_following_tail(),
        "FollowMode::Tail engages tail-follow"
    );
    assert_eq!(
        state.max_offset_for_scrollbar().y,
        px(60.),
        "4×40 content in a 100px viewport"
    );
    let top = state.logical_scroll_top();
    assert_eq!(top.item_ix, 4, "tail-follow pins the scroll top to the end");
    assert_eq!(top.offset_in_item, px(0.));
    assert_eq!(state.is_scrolled_to_end(), Some(true));
}

/// Regression: a row whose rendered height changes between frames without an
/// explicit `remeasure` call (async image load, font swap, lazy markdown
/// reflow) must still re-measure, else the cached height stays stale while the
/// painted element takes its fresh height — the element overflows its slot and
/// overlaps the next row ("weird display") or leaves a gap ("blank region").
/// The list's contract is that it re-measures every visible row each frame; a
/// `remeasure` call only widens the re-measurement to out-of-range rows.
#[gpui::test]
async fn list_remeasures_visible_rows_each_frame(cx: &mut TestAppContext) {
    let (window, body, state) = draw_list(cx, vec![px(40.), px(40.)], px(100.));
    assert_eq!(
        state.max_offset_for_scrollbar().y,
        px(0.),
        "both rows measured at the definite width; fitting content is not scrollable"
    );

    // Grow row 0 to 200px WITHOUT flagging `remeasure` — the production
    // paths that miss a remeasure look exactly like this to the list.
    body.borrow_mut()[0] = px(200.);
    redraw(cx, window.into());

    assert_eq!(
        state.max_offset_for_scrollbar().y,
        px(140.),
        "a visible row's height change must self-correct without remeasure"
    );
}

#[gpui::test]
async fn list_clamps_scroll_anchor_when_visible_row_shrinks(cx: &mut TestAppContext) {
    let (window, body, state) = draw_list(cx, vec![px(800.), px(40.), px(40.)], px(100.));
    state.set_follow_mode(FollowMode::Normal);
    state.scroll_to(gpui::ListOffset {
        item_ix: 0,
        offset_in_item: px(700.),
    });
    redraw(cx, window.into());

    body.borrow_mut()[0] = px(80.);
    redraw(cx, window.into());

    let top = state.logical_scroll_top();
    assert!(
        top.item_ix > 0 || top.offset_in_item <= px(80.),
        "scroll anchor remained outside the shrunken row: {top:?}"
    );
}

struct PersistentMarkdownRow {
    markdown: gpui::Entity<Markdown>,
}

impl Render for PersistentMarkdownRow {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        agent_ui::views::centered(self.markdown.clone())
    }
}

struct MarkdownListProbe {
    state: ListState,
    rows: Vec<gpui::Entity<PersistentMarkdownRow>>,
}

impl Render for MarkdownListProbe {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.rows.clone();
        list(self.state.clone(), move |ix, _window, _cx| {
            div()
                .w_full()
                .min_w_0()
                .flex_shrink_0()
                .debug_selector(move || format!("markdown-list-row-{ix}"))
                .child(rows[ix].clone())
                .into_any_element()
        })
        .w_full()
        .h_full()
        .min_h_0()
        .min_w_0()
    }
}

fn markdown_rows(
    cx: &mut TestAppContext,
    first: gpui::Entity<Markdown>,
) -> (
    gpui::Entity<PersistentMarkdownRow>,
    gpui::Entity<PersistentMarkdownRow>,
) {
    let tail = cx.new(|cx| Markdown::new("markdown-tail", "tail message").theme(cx.theme()));
    (
        cx.new(|_| PersistentMarkdownRow { markdown: first }),
        cx.new(|_| PersistentMarkdownRow { markdown: tail }),
    )
}

const EXPLORE_SUMMARY_WITH_TABLE: &str = r#"3 个 Explore agent 全部成功完成，返回了结构化摘要。测试结果如下：

## 测试结论

**Explore agent 工作正常**——三个并行子代理均完整探索并返回了带文件路径/符号引用的结构化摘要。

## 三个视角的产出对比

| # | 探索角度 | 产出亮点 |
|---|---|---|
| 1 | **整体架构与 crate 布局** | 15 个 workspace 成员、三层主架构 + pi harness 层 + 终端栈 + 7 个支撑 crate 的完整职责表；构建入口（`manox` bin、`crates/manox/src/main.rs`）；GPUI 版本锁定与 patch 说明 |
| 2 | **pi harness 分层**（内核/扩展边界） | 内核 8 大模块 + 8 个显式「缝隙」清单（`AgentTool::requires_approval`、`BashOperations`、`HookPoint`、`BackgroundTaskRegistry` 等）；扩展层 6 类实现如何经缝隙接入；宿主 `pi_engine.rs`/`pi_approval.rs` 的装配点；附 40+ 条符号索引表 |
| 3 | **agent 宿主 + agent-ui UI** | agent 侧 12 大能力模块路径；UI 组件与文件映射；i18n 边界（`en.ftl`/`zh-CN.ftl` + `t()` 调用示例）；5 类运行时配置的读取位置；并指出 CLAUDE.md 与代码的一处偏差（`system_prompt.md` 已退役，现行是 `.tera.md` 双语言模板） |

## 有价值的发现

- 三个 agent 的结论**相互印证一致**：都确认了 `agent → pi-extensions → pi` 依赖链、`pi_engine.rs` 作为宿主装配核心、`ApprovalGatedTool` 为审批门控落点。
- 第 3 个 agent 发现了 **CLAUDE.md 与代码的偏差**（提示词系统已从 `system_prompt.md` 迁移到 `.tera.md` 模板），这类「文档腐化」信号值得留意（按项目规则注释/文档错位即回归，可考虑后续修订 CLAUDE.md）。
- 第 2 个 agent 产出最详尽（含符号索引表），适合作为后续深入阅读的导航。

如需继续，可以再测某个特定视角（如审批链路、compaction、hashline 协议）或让某个 agent 深入跟进某一模块。"#;

/// Real session 465a1ff0 regression. Its three-column GFM table forces flex
/// cells to issue min-content, max-content and definite-width measurements in
/// one layout. Intrinsic probes must not replace the definite shape used by
/// paint or leave the message row at a probe-only height.
#[gpui::test]
async fn real_table_message_height_converges_across_width_changes(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let markdown = cx.new(|cx| {
        Markdown::new("explore-summary-table", EXPLORE_SUMMARY_WITH_TABLE).theme(cx.theme())
    });
    markdown.update(cx, |markdown, cx| markdown.finalize(cx));
    let (first, tail) = markdown_rows(cx, markdown);
    let state = ListState::new(2, ListAlignment::Bottom, px(2048.));
    state.set_follow_mode(FollowMode::Tail);
    let window = cx.open_window(size(px(520.), px(4_000.)), {
        let state = state.clone();
        move |_, _| MarkdownListProbe {
            state,
            rows: vec![first, tail],
        }
    });
    cx.run_until_parked();
    let mut visual = VisualTestContext::from_window(window.into(), cx);
    for _ in 0..2 {
        visual.update(|window, cx| window.draw(cx).clear());
    }
    let narrow = visual
        .debug_bounds("markdown-list-row-0")
        .expect("narrow real-session table row");

    visual.simulate_resize(size(px(1_200.), px(4_000.)));
    visual.run_until_parked();
    for _ in 0..2 {
        visual.update(|window, cx| window.draw(cx).clear());
    }
    let wide = visual
        .debug_bounds("markdown-list-row-0")
        .expect("wide real-session table row");
    let tail = visual
        .debug_bounds("markdown-list-row-1")
        .expect("row following real-session table");

    assert!(
        wide.size.height <= narrow.size.height + px(1.),
        "widening made the table message taller: narrow={narrow:?}, wide={wide:?}"
    );
    assert!(
        wide.bottom() <= tail.top(),
        "real-session table message overlaps its following row: wide={wide:?}, tail={tail:?}"
    );

    visual.simulate_resize(size(px(520.), px(4_000.)));
    visual.run_until_parked();
    for _ in 0..2 {
        visual.update(|window, cx| window.draw(cx).clear());
    }
    let narrow_again = visual
        .debug_bounds("markdown-list-row-0")
        .expect("narrow real-session table row after round trip");

    visual.simulate_resize(size(px(1_200.), px(4_000.)));
    visual.run_until_parked();
    for _ in 0..2 {
        visual.update(|window, cx| window.draw(cx).clear());
    }
    let wide_again = visual
        .debug_bounds("markdown-list-row-0")
        .expect("wide real-session table row after round trip");
    let tail_again = visual
        .debug_bounds("markdown-list-row-1")
        .expect("row following real-session table after round trip");

    assert!(
        (narrow_again.size.height - narrow.size.height).abs() <= px(1.),
        "narrow height changed after a width round trip: first={narrow:?}, again={narrow_again:?}"
    );
    assert!(
        (wide_again.size.height - wide.size.height).abs() <= px(1.),
        "wide height changed after a width round trip: first={wide:?}, again={wide_again:?}"
    );
    assert!(
        wide_again.bottom() <= tail_again.top(),
        "round-tripped table message overlaps its following row: wide={wide_again:?}, tail={tail_again:?}"
    );
}

#[gpui::test]
async fn list_measures_persistent_wrapped_markdown_rows(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let long = (0..80)
        .map(|ix| format!("- item {ix}: 这是一段需要在确定宽度下换行的 markdown 内容。"))
        .collect::<Vec<_>>()
        .join("\n");
    let markdown = cx.new(|cx| Markdown::new("long-markdown", long).theme(cx.theme()));
    let (first, tail) = markdown_rows(cx, markdown);
    let state = ListState::new(2, ListAlignment::Bottom, px(2048.));
    state.set_follow_mode(FollowMode::Tail);
    let window = cx.open_window(size(px(760.), px(600.)), {
        let state = state.clone();
        move |_, _| MarkdownListProbe {
            state,
            rows: vec![first, tail],
        }
    });
    cx.run_until_parked();
    let mut visual = VisualTestContext::from_window(window.into(), cx);
    visual.update(|window, cx| window.draw(cx).clear());

    let first = visual
        .debug_bounds("markdown-list-row-0")
        .expect("first row");
    let tail = visual
        .debug_bounds("markdown-list-row-1")
        .expect("tail row");
    assert!(
        first.size.height > px(600.),
        "long markdown was under-measured: {first:?}"
    );
    assert!(
        first.bottom() <= tail.top(),
        "markdown rows overlap: first={first:?}, tail={tail:?}"
    );
}

#[gpui::test]
async fn list_remeasures_streaming_markdown_child_growth(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let prefix = "# Plan\n\nInitial paragraph.\n".to_string();
    let markdown = cx.new(|cx| {
        Markdown::new("streaming-plan", prefix.clone())
            .theme(cx.theme())
            .streaming(true)
    });
    let (first, tail) = markdown_rows(cx, markdown.clone());
    let state = ListState::new(2, ListAlignment::Bottom, px(2048.));
    state.set_follow_mode(FollowMode::Tail);
    let window = cx.open_window(size(px(760.), px(600.)), {
        let state = state.clone();
        move |_, _| MarkdownListProbe {
            state,
            rows: vec![first, tail],
        }
    });
    cx.run_until_parked();
    let mut visual = VisualTestContext::from_window(window.into(), cx);
    visual.update(|window, cx| window.draw(cx).clear());

    let suffix = (0..100)
        .map(|ix| format!("\n## Section {ix}\n\n- streamed wrapped content {ix}\n"))
        .collect::<String>();
    markdown.update(&mut visual.cx, |markdown, cx| {
        markdown.replace(format!("{prefix}{suffix}"), cx)
    });
    visual.run_until_parked();
    visual.update(|window, cx| window.draw(cx).clear());

    let first = visual
        .debug_bounds("markdown-list-row-0")
        .expect("plan row");
    let tail = visual
        .debug_bounds("markdown-list-row-1")
        .expect("tail row");
    assert!(
        first.size.height > px(600.),
        "streamed markdown was under-measured: {first:?}"
    );
    assert!(
        first.bottom() <= tail.top(),
        "streamed markdown overlaps tail: first={first:?}, tail={tail:?}"
    );
}

struct MessageItemListProbe {
    state: ListState,
    rows: Vec<gpui::Entity<MessageItem>>,
    width_invalidator: MessageListWidthInvalidator,
}

impl MessageItemListProbe {
    fn new(state: ListState, rows: Vec<gpui::Entity<MessageItem>>) -> Self {
        Self {
            state,
            rows,
            width_invalidator: MessageListWidthInvalidator::default(),
        }
    }
}

impl Render for MessageItemListProbe {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.rows.clone();
        let mono_family = cx.theme().mono_font_family.clone();
        let state = self.state.clone();
        let width_state = state.clone();
        let width_invalidator = self.width_invalidator.clone();
        div()
            .size_full()
            .min_h_0()
            .min_w_0()
            .child(
                list(state, move |ix, _window, _cx| {
                    div()
                        .w_full()
                        .pt_1()
                        .pb_4()
                        .min_w_0()
                        .flex_shrink_0()
                        .debug_selector(move || format!("message-item-list-row-{ix}"))
                        .child(rows[ix].clone())
                        .into_any_element()
                })
                .w_full()
                .h_full()
                .min_h_0()
                .min_w_0()
                .font_family(mono_family)
                .font_weight(gpui::FontWeight::LIGHT),
            )
            .on_prepaint(move |bounds, window, _cx| {
                if width_invalidator.update(bounds.size.width, &width_state) {
                    window.refresh();
                }
            })
    }
}

#[gpui::test]
async fn list_remeasures_real_message_item_when_markdown_child_grows(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let prefix = "# Plan\n\nInitial paragraph.\n".to_string();
    let weak = gpui::WeakEntity::<Workspace>::new_invalid();
    let plan = cx.new(|_| {
        MessageItem::new(
            ConvItem::Assistant {
                text: prefix.clone(),
                streaming: true,
                token_usage: None,
                activity_secs: None,
            },
            "DeepSeek".into(),
            0,
            weak.clone(),
        )
    });
    let tail = cx.new(|_| {
        MessageItem::new(
            ConvItem::Assistant {
                text: "tail message".into(),
                streaming: false,
                token_usage: None,
                activity_secs: None,
            },
            "DeepSeek".into(),
            1,
            weak,
        )
    });
    let state = ListState::new(2, ListAlignment::Bottom, px(2048.));
    state.set_follow_mode(FollowMode::Tail);
    let window = cx.open_window(size(px(760.), px(600.)), {
        let state = state.clone();
        let plan = plan.clone();
        move |_, _| MessageItemListProbe::new(state, vec![plan, tail])
    });
    cx.run_until_parked();
    let mut visual = VisualTestContext::from_window(window.into(), cx);
    visual.update(|window, cx| window.draw(cx).clear());

    let suffix = (0..100)
        .map(|ix| format!("\n## Section {ix}\n\n- streamed wrapped content for item {ix}\n"))
        .collect::<String>();
    let full = format!("{prefix}{suffix}");
    plan.update(&mut visual.cx, |item, cx| {
        if let ConvItem::Assistant { text, .. } = item.kind_mut() {
            *text = full.clone();
        }
        // Only the persistent Markdown child is notified by `update_text`.
        item.update_text(&full, cx);
    });
    visual.run_until_parked();
    visual.update(|window, cx| window.draw(cx).clear());

    let plan = visual
        .debug_bounds("message-item-list-row-0")
        .expect("plan message row");
    let tail = visual
        .debug_bounds("message-item-list-row-1")
        .expect("tail message row");
    assert!(
        plan.size.height > px(600.),
        "plan row was under-measured: {plan:?}"
    );
    assert!(
        plan.bottom() <= tail.top(),
        "MessageItem rows overlap: plan={plan:?}, tail={tail:?}"
    );
}

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
            activity_secs: None,
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
            activity_secs: None,
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

fn assert_fixture_activity_is_contained(visual: &mut VisualTestContext) {
    let row = visual
        .debug_bounds("message-item-list-row-2")
        .expect("thinking message row");
    let tree = visual
        .debug_bounds("message-overflow-activity-tree-2")
        .expect("activity tree");
    assert!(
        tree.top() >= row.top() && tree.bottom() <= row.bottom(),
        "painted activity escaped its cached list row: row={row:?}, tree={tree:?}"
    );
    let mut previous = None;
    for selector in [
        "message-overflow-activity-entry-2-0",
        "message-overflow-activity-entry-2-1",
    ] {
        let entry = visual.debug_bounds(selector).expect("activity entry");
        assert!(
            entry.top() >= tree.top() && entry.bottom() <= tree.bottom(),
            "activity entry escaped its tree: tree={tree:?}, entry={entry:?}"
        );
        if let Some(previous) = previous {
            assert!(
                previous <= entry.top(),
                "painted activity entries overlap: previous bottom={previous:?}, entry={entry:?}"
            );
        }
        previous = Some(entry.bottom());
    }
}

#[gpui::test]
async fn production_rows_do_not_overlap_on_consecutive_frames(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let rows = production_rows(cx);
    let state = ListState::new(rows.len(), ListAlignment::Bottom, px(2048.));
    state.set_follow_mode(FollowMode::Tail);
    let window = cx.open_window(size(px(760.), px(4000.)), {
        let state = state.clone();
        move |_, _| MessageItemListProbe::new(state, rows)
    });
    cx.run_until_parked();
    let mut visual = VisualTestContext::from_window(window.into(), cx);
    for _ in 0..2 {
        visual.update(|window, cx| window.draw(cx).clear());
        let rows = (0..4)
            .map(|ix| {
                visual
                    .debug_bounds(match ix {
                        0 => "message-item-list-row-0",
                        1 => "message-item-list-row-1",
                        2 => "message-item-list-row-2",
                        _ => "message-item-list-row-3",
                    })
                    .expect("production row")
            })
            .collect::<Vec<_>>();
        for pair in rows.windows(2) {
            assert!(
                pair[0].bottom() <= pair[1].top(),
                "production rows overlap: {:?} then {:?}",
                pair[0],
                pair[1]
            );
        }
        assert_fixture_activity_is_contained(&mut visual);
    }
}

#[gpui::test]
async fn production_list_recovers_after_narrow_width_without_blank_range(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let rows = production_rows(cx);
    let state = ListState::new(rows.len(), ListAlignment::Bottom, px(2048.));
    state.set_follow_mode(FollowMode::Tail);
    // First expose every row at a realistic narrow width so GPUI caches all
    // narrow wrapped heights. Then reduce only the viewport height, moving the
    // leading rows offscreen without changing those cached measurements.
    let window = cx.open_window(size(px(320.), px(30_000.)), {
        let state = state.clone();
        move |_, _| MessageItemListProbe::new(state, rows)
    });
    cx.run_until_parked();
    let mut visual = VisualTestContext::from_window(window.into(), cx);
    for _ in 0..2 {
        visual.update(|window, cx| window.draw(cx).clear());
    }
    visual.simulate_resize(size(px(320.), px(600.)));
    visual.run_until_parked();
    visual.update(|window, cx| window.draw(cx).clear());
    let narrow_scroll_range = state.max_offset_for_scrollbar().y;
    assert!(
        narrow_scroll_range > px(1_000.),
        "fixture was not tall enough"
    );

    visual.simulate_resize(size(px(1_200.), px(600.)));
    visual.run_until_parked();
    for _ in 0..2 {
        visual.update(|window, cx| window.draw(cx).clear());
    }
    let wide_scroll_range = state.max_offset_for_scrollbar().y;
    assert!(
        wide_scroll_range < narrow_scroll_range * 0.8,
        "off-screen narrow-width heights survived widening: narrow={narrow_scroll_range:?}, wide={wide_scroll_range:?}"
    );
    let tail = visual
        .debug_bounds("message-item-list-row-3")
        .expect("tail row is visible after resize recovery");
    assert!(tail.top() >= px(0.) && tail.bottom() <= px(600.));
    assert!(state.is_scrolled_to_end().unwrap_or(true));
}

struct TerminalListProbe {
    state: ListState,
    panel: gpui::Entity<TerminalPanel>,
    tail: gpui::Entity<Markdown>,
}

impl Render for TerminalListProbe {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let panel = self.panel.clone();
        let tail = self.tail.clone();
        list(self.state.clone(), move |ix, _window, _cx| {
            div()
                .w_full()
                .min_w_0()
                .flex_shrink_0()
                .debug_selector(move || format!("terminal-list-row-{ix}"))
                .child(if ix == 0 {
                    panel.clone().into_any_element()
                } else {
                    tail.clone().into_any_element()
                })
                .into_any_element()
        })
        .w_full()
        .h_full()
        .min_h_0()
        .min_w_0()
    }
}

#[gpui::test]
async fn list_measures_persistent_terminal_panel_rows(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let output = (0..100)
        .map(|ix| format!("line {ix}: terminal output with enough text to wrap at the list width"))
        .collect::<Vec<_>>()
        .join("\n");
    let panel = cx.new(|cx| {
        let mut panel = TerminalPanel::new(PanelKind::Plain, None, None, cx.theme());
        panel.set_streaming(true, cx);
        panel.set_output(output, cx);
        panel
    });
    let tail = cx.new(|cx| Markdown::new("terminal-tail", "assistant tail").theme(cx.theme()));
    let state = ListState::new(2, ListAlignment::Bottom, px(2048.));
    state.set_follow_mode(FollowMode::Tail);
    let window = cx.open_window(size(px(760.), px(600.)), {
        let state = state.clone();
        move |_, _| TerminalListProbe { state, panel, tail }
    });
    cx.run_until_parked();
    let mut visual = VisualTestContext::from_window(window.into(), cx);
    visual.update(|window, cx| window.draw(cx).clear());

    let panel = visual
        .debug_bounds("terminal-list-row-0")
        .expect("terminal panel row");
    let tail = visual
        .debug_bounds("terminal-list-row-1")
        .expect("terminal tail row");
    assert!(
        panel.size.height > px(600.),
        "terminal panel was under-measured: {panel:?}"
    );
    assert!(
        panel.bottom() <= tail.top(),
        "terminal panel overlaps tail: panel={panel:?}, tail={tail:?}"
    );
}

#[gpui::test]
async fn terminal_load_more_remeasures_row_without_overlap(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let output = (0..100)
        .map(|ix| format!("line {ix}: paginated terminal output"))
        .collect::<Vec<_>>()
        .join("\n");
    let panel = cx.new(|cx| {
        let mut panel = TerminalPanel::new(PanelKind::Plain, None, None, cx.theme());
        panel.set_output(output, cx);
        panel
    });
    let tail = cx.new(|cx| Markdown::new("load-more-tail", "assistant tail").theme(cx.theme()));
    let state = ListState::new(2, ListAlignment::Bottom, px(2048.));
    state.set_follow_mode(FollowMode::Tail);
    let window = cx.open_window(size(px(760.), px(600.)), {
        let state = state.clone();
        move |_, _| TerminalListProbe { state, panel, tail }
    });
    cx.run_until_parked();
    let mut visual = VisualTestContext::from_window(window.into(), cx);
    visual.update(|window, cx| window.draw(cx).clear());
    let before = visual
        .debug_bounds("terminal-list-row-0")
        .expect("terminal row before load more");
    let load_more = gpui::point(before.center().x, before.bottom() - px(10.));

    visual.simulate_mouse_down(load_more, MouseButton::Left, Modifiers::none());
    visual.simulate_mouse_up(load_more, MouseButton::Left, Modifiers::none());
    visual.run_until_parked();
    visual.update(|window, cx| window.draw(cx).clear());

    let after = visual
        .debug_bounds("terminal-list-row-0")
        .expect("terminal row after load more");
    let tail = visual
        .debug_bounds("terminal-list-row-1")
        .expect("tail row after load more");
    assert!(
        after.size.height > before.size.height,
        "load more did not grow the terminal row"
    );
    assert!(
        after.bottom() <= tail.top(),
        "expanded terminal row overlaps tail: panel={after:?}, tail={tail:?}"
    );
}
