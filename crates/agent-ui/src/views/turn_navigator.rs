//! Searchable navigation over the current thread's user turns.

use agent::i18n;
use gpui::{
    App, AppContext as _, ClipboardItem, Context, Entity, EventEmitter, IntoElement, Render,
    SharedString, Subscription, Task, Window, prelude::*, px,
};
use gpui_component::{
    ActiveTheme as _, IndexPath, Sizable as _, Size, WindowExt as _,
    list::{List, ListDelegate, ListEvent, ListItem, ListState},
    notification::Notification,
    v_flex,
};

use crate::CopySelectedTurn;
use crate::conversation::ConvItem;

const SEARCH_HEIGHT: f32 = 36.0;
const ROW_HEIGHT: f32 = 28.0;
const EMPTY_HEIGHT: f32 = 72.0;
const MAX_PANEL_HEIGHT: f32 = 360.0;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TurnEntry {
    pub item_ix: usize,
    pub text: String,
    pub display: String,
    search_text: String,
}

impl TurnEntry {
    fn new(item_ix: usize, text: &str, has_images: bool) -> Self {
        let collapsed = collapse_whitespace(text);
        let display = if !collapsed.is_empty() {
            collapsed
        } else if has_images {
            i18n::t("turn-navigator-attachment-only").to_string()
        } else {
            i18n::t("turn-navigator-empty-message").to_string()
        };
        Self {
            item_ix,
            text: text.to_string(),
            display,
            search_text: text.to_lowercase(),
        }
    }
}

pub(crate) fn collect_user_turns<'a>(
    items: impl Iterator<Item = (usize, &'a ConvItem)>,
) -> Vec<TurnEntry> {
    let mut turns: Vec<_> = items
        .filter_map(|(item_ix, item)| match item {
            ConvItem::User { text, images, .. } => {
                Some(TurnEntry::new(item_ix, text, !images.is_empty()))
            }
            _ => None,
        })
        .collect();
    turns.reverse();
    turns
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn filter_turns(turns: &[TurnEntry], query: &str) -> Vec<usize> {
    let query = query.to_lowercase();
    if query.is_empty() {
        return (0..turns.len()).collect();
    }
    turns
        .iter()
        .enumerate()
        .filter_map(|(ix, turn)| turn.search_text.contains(&query).then_some(ix))
        .collect()
}

struct TurnListDelegate {
    all: Vec<TurnEntry>,
    filtered: Vec<usize>,
    selected: Option<IndexPath>,
}

impl TurnListDelegate {
    fn new(turns: Vec<TurnEntry>) -> Self {
        let filtered = (0..turns.len()).collect();
        Self {
            filtered,
            all: turns,
            selected: None,
        }
    }

    fn selected_entry(&self) -> Option<&TurnEntry> {
        let ix = self.selected?;
        self.entry_at(ix.row)
    }

    fn entry_at(&self, row: usize) -> Option<&TurnEntry> {
        self.filtered
            .get(row)
            .and_then(|entry_ix| self.all.get(*entry_ix))
    }
}

impl ListDelegate for TurnListDelegate {
    type Item = ListItem;

    fn perform_search(
        &mut self,
        query: &str,
        window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> Task<()> {
        self.filtered = filter_turns(&self.all, query);
        let selected = (!self.filtered.is_empty()).then(IndexPath::default);
        cx.defer_in(window, move |state, window, cx| {
            state.set_selected_index(selected, window, cx);
            cx.notify();
        });
        Task::ready(())
    }

    fn items_count(&self, _section: usize, _cx: &App) -> usize {
        self.filtered.len()
    }

    fn render_item(
        &mut self,
        ix: IndexPath,
        _window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> Option<Self::Item> {
        let turn = self.entry_at(ix.row)?;
        Some(
            ListItem::new(("turn-navigator-row", turn.item_ix))
                .h(px(ROW_HEIGHT))
                .mx_1()
                .px_2()
                .rounded(cx.theme().radius)
                .child(
                    gpui::div()
                        .w_full()
                        .min_w_0()
                        .truncate()
                        .text_sm()
                        .debug_selector(|| format!("TURN_NAVIGATOR_ROW_{}", turn.item_ix))
                        .child(SharedString::from(turn.display.clone())),
                ),
        )
    }

    fn render_empty(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> impl IntoElement {
        let key = if self.all.is_empty() {
            "turn-navigator-empty"
        } else {
            "turn-navigator-no-results"
        };
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .child(i18n::t(key))
    }

    fn set_selected_index(
        &mut self,
        ix: Option<IndexPath>,
        _window: &mut Window,
        _cx: &mut Context<ListState<Self>>,
    ) {
        self.selected = ix;
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TurnNavigatorEvent {
    Navigate { item_ix: usize },
    Dismiss,
}

pub(crate) struct TurnNavigator {
    list: Entity<ListState<TurnListDelegate>>,
    _list_sub: Subscription,
    #[cfg(test)]
    last_event: Option<TurnNavigatorEvent>,
}

impl TurnNavigator {
    pub fn new(turns: Vec<TurnEntry>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let has_turns = !turns.is_empty();
        let list =
            cx.new(|cx| ListState::new(TurnListDelegate::new(turns), window, cx).searchable(true));
        if has_turns {
            list.update(cx, |state, cx| {
                state.set_selected_index(Some(IndexPath::default()), window, cx);
            });
        }
        let _list_sub = cx.subscribe_in(&list, window, Self::on_list_event);
        Self {
            list,
            _list_sub,
            #[cfg(test)]
            last_event: None,
        }
    }

    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        self.list.update(cx, |list, cx| list.focus(window, cx));
    }

    pub fn panel_height(&self, cx: &App) -> gpui::Pixels {
        let rows = self.list.read(cx).delegate().filtered.len();
        let body = if rows == 0 {
            EMPTY_HEIGHT
        } else {
            rows as f32 * ROW_HEIGHT
        };
        px((SEARCH_HEIGHT + body).min(MAX_PANEL_HEIGHT))
    }

    fn on_list_event(
        &mut self,
        list: &Entity<ListState<TurnListDelegate>>,
        event: &ListEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        #[cfg(test)]
        {
            self.last_event = match event {
                ListEvent::Confirm(ix) => {
                    let delegate = list.read(cx).delegate();
                    delegate
                        .entry_at(ix.row)
                        .map(|turn| TurnNavigatorEvent::Navigate {
                            item_ix: turn.item_ix,
                        })
                }
                ListEvent::Cancel => Some(TurnNavigatorEvent::Dismiss),
                ListEvent::Select(_) => self.last_event.clone(),
            };
        }
        match event {
            ListEvent::Confirm(ix) => {
                let delegate = list.read(cx).delegate();
                if let Some(turn) = delegate.entry_at(ix.row) {
                    cx.emit(TurnNavigatorEvent::Navigate {
                        item_ix: turn.item_ix,
                    });
                }
            }
            ListEvent::Cancel => cx.emit(TurnNavigatorEvent::Dismiss),
            ListEvent::Select(_) => {}
        }
    }

    fn copy_selected(&mut self, _: &CopySelectedTurn, window: &mut Window, cx: &mut Context<Self>) {
        let Some(turn) = self.list.read(cx).delegate().selected_entry() else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(turn.text.clone()));
        window.push_notification(Notification::success(i18n::t("turn-navigator-copied")), cx);
        cx.stop_propagation();
    }
}

impl EventEmitter<TurnNavigatorEvent> for TurnNavigator {}

impl Render for TurnNavigator {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .id("turn-navigator")
            .key_context("TurnNavigator")
            .size_full()
            .overflow_hidden()
            .bg(cx.theme().popover)
            .text_color(cx.theme().popover_foreground)
            .on_action(cx.listener(Self::copy_selected))
            .child(
                List::new(&self.list)
                    .with_size(Size::Small)
                    .scrollbar_visible(false)
                    .search_placeholder(i18n::t("turn-navigator-search-placeholder")),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Modifiers, TestAppContext, size};
    use gpui_component::Root;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn user(text: &str) -> ConvItem {
        ConvItem::User {
            text: text.to_string(),
            images: Vec::new(),
            meta: None,
        }
    }

    fn assistant(text: &str) -> ConvItem {
        ConvItem::Assistant {
            text: text.to_string(),
            streaming: false,
            token_usage: None,
        }
    }

    #[test]
    fn collects_user_turns_newest_first_with_message_indices() {
        let items = [user("old"), assistant("reply"), user("new")];
        let turns = collect_user_turns(items.iter().enumerate());
        assert_eq!(turns.iter().map(|t| t.item_ix).collect::<Vec<_>>(), [2, 0]);
        assert_eq!(
            turns.iter().map(|t| t.text.as_str()).collect::<Vec<_>>(),
            ["new", "old"]
        );
    }

    #[test]
    fn collapses_multiline_whitespace_for_single_line_display() {
        assert_eq!(
            collapse_whitespace("  first\n\n second\tthird  "),
            "first second third"
        );
    }

    #[test]
    fn attachment_only_turn_uses_localized_placeholder() {
        let turn = TurnEntry::new(3, "", true);
        assert_eq!(turn.display, i18n::t("turn-navigator-attachment-only"));
    }

    #[test]
    fn filters_full_text_case_insensitively_without_reordering() {
        let turns = vec![
            TurnEntry::new(8, "Latest line\nHidden Needle", false),
            TurnEntry::new(3, "older NEEDLE", false),
            TurnEntry::new(1, "unrelated", false),
        ];
        let filtered = filter_turns(&turns, "needle");
        assert_eq!(
            filtered
                .iter()
                .map(|ix| turns[*ix].item_ix)
                .collect::<Vec<_>>(),
            [8, 3]
        );
    }

    #[gpui::test]
    fn keyboard_mouse_search_navigation_copy_and_dismiss(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let slot = Rc::new(RefCell::new(None));
        let slot_for_window = slot.clone();
        let turns = vec![
            TurnEntry::new(8, "latest needle", false),
            TurnEntry::new(3, "older needle\nwith detail", false),
            TurnEntry::new(1, "unrelated", false),
        ];
        let (_root, cx) = cx.add_window_view(move |window, cx| {
            let navigator = cx.new(|cx| TurnNavigator::new(turns, window, cx));
            *slot_for_window.borrow_mut() = Some(navigator.clone());
            Root::new(navigator, window, cx)
        });
        cx.simulate_resize(size(px(640.), px(480.)));
        let navigator = slot
            .borrow()
            .as_ref()
            .expect("navigator initialized")
            .clone();
        cx.update(|window, cx| {
            navigator.update(cx, |navigator, cx| navigator.focus(window, cx));
        });

        cx.simulate_input("needle");
        navigator.read_with(cx, |navigator, cx| {
            let delegate = navigator.list.read(cx).delegate();
            assert_eq!(delegate.filtered.len(), 2);
            assert_eq!(delegate.selected_entry().map(|turn| turn.item_ix), Some(8));
        });

        cx.simulate_keystrokes("down enter");
        navigator.read_with(cx, |navigator, _| {
            assert_eq!(
                navigator.last_event,
                Some(TurnNavigatorEvent::Navigate { item_ix: 3 })
            );
        });

        navigator.update(cx, |navigator, _| navigator.last_event = None);
        let older_row = cx
            .debug_bounds("TURN_NAVIGATOR_ROW_3")
            .expect("filtered row rendered");
        cx.simulate_click(older_row.center(), Modifiers::default());
        navigator.read_with(cx, |navigator, _| {
            assert_eq!(
                navigator.last_event,
                Some(TurnNavigatorEvent::Navigate { item_ix: 3 })
            );
        });

        cx.dispatch_action(CopySelectedTurn);
        let copied = cx
            .update(|_window, cx| cx.read_from_clipboard())
            .and_then(|item| item.text());
        assert_eq!(copied.as_deref(), Some("older needle\nwith detail"));

        cx.simulate_keystrokes("escape");
        navigator.read_with(cx, |navigator, _| {
            assert_eq!(navigator.last_event, Some(TurnNavigatorEvent::Dismiss));
        });
    }
}
