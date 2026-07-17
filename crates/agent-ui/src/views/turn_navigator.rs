//! Searchable navigation over the current thread's user turns.
//!
//! Renders a centered popup panel with a search input and a scrollable list of
//! filtered user turns. The visual style (hover/selected backgrounds, container
//! chrome) is shared with the slash-command completion popover via
//! `views::popup_menu`.

use agent::i18n;
use gpui::{
    App, AppContext as _, ClipboardItem, Context, Entity, EventEmitter, IntoElement,
    Render, ScrollHandle, SharedString, Subscription, Window, prelude::*,
};
use gpui_component::{
    ActiveTheme as _, WindowExt as _,
    input::{Input, InputEvent, InputState},
    notification::Notification,
    v_flex,
};

use crate::CopySelectedTurn;
use crate::conversation::ConvItem;
use crate::views::popup_menu::{
    self, EMPTY_HEIGHT, MAX_LIST_HEIGHT, ROW_HEIGHT, SEARCH_HEIGHT,
};

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TurnNavigatorEvent {
    Navigate { item_ix: usize },
    Dismiss,
}

pub(crate) struct TurnNavigator {
    all: Vec<TurnEntry>,
    filtered: Vec<usize>,
    selected: usize,
    search: Entity<InputState>,
    scroll_handle: ScrollHandle,
    _search_sub: Subscription,
    #[cfg(test)]
    last_event: Option<TurnNavigatorEvent>,
}

impl TurnNavigator {
    pub fn new(turns: Vec<TurnEntry>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| {
            InputState::new(window, cx).placeholder(i18n::t("turn-navigator-search-placeholder"))
        });
        let filtered = (0..turns.len()).collect();
        let _search_sub = cx.subscribe_in(&search, window, Self::on_search_event);
        Self {
            all: turns,
            filtered,
            selected: 0,
            search,
            scroll_handle: ScrollHandle::new(),
            _search_sub,
            #[cfg(test)]
            last_event: None,
        }
    }

    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        self.search.update(cx, |s, cx| s.focus(window, cx));
    }

    pub fn panel_height(&self, _cx: &App) -> gpui::Pixels {
        let rows = self.filtered.len();
        let body = if rows == 0 {
            EMPTY_HEIGHT
        } else {
            let height_px = ROW_HEIGHT * rows as f32;
            if height_px > MAX_LIST_HEIGHT {
                MAX_LIST_HEIGHT
            } else {
                height_px
            }
        };
        SEARCH_HEIGHT + body
    }

    fn on_search_event(
        &mut self,
        search: &Entity<InputState>,
        event: &InputEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::Change => {
                let query = search.read(cx).value().to_string();
                self.filtered = filter_turns(&self.all, &query);
                // Keep selection within bounds; reset to first match.
                self.selected = if self.filtered.is_empty() { 0 } else { 0 };
                cx.notify();
            }
            InputEvent::PressEnter { shift: false, .. } => {
                self.confirm(cx);
            }
            _ => {}
        }
    }

    fn move_selection(&mut self, delta: i32) {
        if self.filtered.is_empty() {
            return;
        }
        let n = self.filtered.len() as i32;
        let mut next = self.selected as i32 + delta;
        next = ((next % n) + n) % n;
        self.selected = next as usize;
        // Scroll the selected item into view.
        self.scroll_handle.scroll_to_item(self.selected);
    }

    fn confirm(&mut self, cx: &mut Context<Self>) {
        let Some(&entry_ix) = self.filtered.get(self.selected) else {
            return;
        };
        let Some(turn) = self.all.get(entry_ix) else {
            return;
        };
        #[cfg(test)]
        {
            self.last_event = Some(TurnNavigatorEvent::Navigate {
                item_ix: turn.item_ix,
            });
        }
        cx.emit(TurnNavigatorEvent::Navigate {
            item_ix: turn.item_ix,
        });
    }

    fn dismiss(&mut self, cx: &mut Context<Self>) {
        #[cfg(test)]
        {
            self.last_event = Some(TurnNavigatorEvent::Dismiss);
        }
        cx.emit(TurnNavigatorEvent::Dismiss);
    }

    fn navigate_up(&mut self, _: &crate::CompletionUp, _window: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(-1);
        cx.notify();
        cx.stop_propagation();
    }

    fn navigate_down(&mut self, _: &crate::CompletionDown, _window: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(1);
        cx.notify();
        cx.stop_propagation();
    }

    fn on_dismiss(&mut self, _: &crate::CompletionDismiss, _window: &mut Window, cx: &mut Context<Self>) {
        self.dismiss(cx);
        cx.stop_propagation();
    }
    fn copy_selected(&mut self, _: &CopySelectedTurn, window: &mut Window, cx: &mut Context<Self>) {
        let Some(&entry_ix) = self.filtered.get(self.selected) else {
            return;
        };
        let Some(turn) = self.all.get(entry_ix) else {
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
        let theme = cx.theme().clone();
        let filtered = self.filtered.clone();
        let all_for_closure = self.all.clone();
        let all_empty = self.all.is_empty();
        let selected = self.selected;
        let scroll_handle = self.scroll_handle.clone();
        let hover = popup_menu::hover_bg(&theme);
        let selected_bg = popup_menu::selected_bg(&theme);
        let radius = theme.radius;
        let is_empty = filtered.is_empty();
        let entity = cx.entity();

        let list = v_flex()
            .id("turn-navigator-list")
            .w_full()
            .max_h(MAX_LIST_HEIGHT)
            .overflow_y_scroll()
            .track_scroll(&scroll_handle)
            .min_w_0()
            .children(filtered.iter().enumerate().map(move |(row_ix, &entry_ix)| {
                let entity = entity.clone();
                let is_selected = row_ix == selected;
                let Some(turn) = all_for_closure.get(entry_ix) else {
                    return gpui::div().into_any_element();
                };
                let display = turn.display.clone();
                let item_ix = turn.item_ix;
                let navigate_item_ix = turn.item_ix;

                let mut row = gpui::div()
                    .id(("turn-navigator-row", row_ix))
                    .w_full()
                    .h(ROW_HEIGHT)
                    .flex()
                    .items_center()
                    .px_2()
                    .rounded(radius)
                    .cursor_pointer()
                    .hover(move |s| s.bg(hover));

                if is_selected {
                    row = row.bg(selected_bg);
                }

                row.child(
                    gpui::div()
                        .w_full()
                        .min_w_0()
                        .truncate()
                        .text_sm()
                        .debug_selector(|| format!("TURN_NAVIGATOR_ROW_{}", item_ix))
                        .child(SharedString::from(display)),
                )
                .on_click(move |_, _, cx| {
                    entity.update(cx, |this, cx| {
                        this.selected = row_ix;
                        cx.emit(TurnNavigatorEvent::Navigate { item_ix: navigate_item_ix });
                    });
                })
                .into_any_element()
            }));

        let body = if is_empty {
            popup_menu::render_empty_state(
                &theme,
                if all_empty {
                    i18n::t("turn-navigator-empty")
                } else {
                    i18n::t("turn-navigator-no-results")
                },
            )
            .into_any_element()
        } else {
            list.into_any_element()
        };

        v_flex()
            .id("turn-navigator")
            .key_context("TurnNavigator")
            .w_full()
            .overflow_hidden()
            .bg(theme.popover)
            .text_color(theme.popover_foreground)
            .on_action(cx.listener(Self::navigate_up))
            .on_action(cx.listener(Self::navigate_down))
            .on_action(cx.listener(Self::on_dismiss))
            .on_action(cx.listener(Self::copy_selected))
            .child(
                gpui::div()
                    .h(SEARCH_HEIGHT)
                    .w_full()
                    .px_2()
                    .flex()
                    .items_center()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        gpui::div()
                            .w_full()
                            .h_full()
                            .child(Input::new(&self.search).appearance(false))
                    ),
            )
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Modifiers, TestAppContext, px, size};
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
            activity_summary: None,
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
        navigator.read_with(cx, |navigator, _cx| {
            assert_eq!(navigator.filtered.len(), 2);
            assert_eq!(navigator.selected, 0);
            let entry_ix = navigator.filtered[0];
            assert_eq!(navigator.all[entry_ix].item_ix, 8);
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
            .debug_bounds("TURN_NAVIGATOR_ROW_0")
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
