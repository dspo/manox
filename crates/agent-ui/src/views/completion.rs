//! Composer typeahead completion popover.
//!
//! A `/` or `@` typed in the input triggers a filtered list anchored above the
//! composer. The list is a pure render overlay — it never grabs focus, so the
//! `InputState` keeps focus and the query filters live on every keystroke.
//! Keyboard navigation (up/down/enter/tab/escape) is intercepted by Workspace
//! keybindings whose context predicate `completion == open > Input` ties the
//! input while the popover is open; see `main.rs` and `workspace.rs`.

use std::rc::Rc;

use agent::i18n;
use agent::{agent_def, skill};
use gpui::{AnyElement, App, ListAlignment, ListState, SharedString, Window, prelude::*, px};
use gpui_component::{Icon, IconName, Sizable as _, Theme, h_flex, v_flex};

use crate::slash_command::SlashCommandRegistry;

/// What a completion row represents — drives its icon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompletionKind {
    Command,
    Skill,
    Agent,
}

impl CompletionKind {
    fn icon(self) -> IconName {
        match self {
            Self::Command => IconName::SquareTerminal,
            Self::Skill => IconName::Frame,
            Self::Agent => IconName::Bot,
        }
    }

    fn tag_key(self) -> &'static str {
        match self {
            Self::Command => "completion-tag-command",
            Self::Skill => "completion-tag-skill",
            Self::Agent => "completion-tag-agent",
        }
    }
}

/// One row in the popover. `name` is the bare command/skill/agent name (no
/// leading trigger); the trigger is prepended at render time.
#[derive(Clone, PartialEq)]
pub struct CompletionItem {
    pub name: SharedString,
    pub description: SharedString,
    pub kind: CompletionKind,
}

/// A trigger hit: where the trigger char sits and the query typed after it.
pub struct Detection {
    pub trigger: char,
    pub token_start: usize,
    pub query: String,
}

/// Decide whether the caret sits inside an active trigger token.
///
/// Scans back from `cursor` to the previous whitespace; the token is
/// `value[token_start..cursor]`. It is a trigger when its first char is `/` or
/// `@` and the caret is within that token (cursor not past the next whitespace).
/// `cursor` is a byte offset and must be on a char boundary within `value`.
pub fn detect(value: &str, cursor: usize) -> Option<Detection> {
    if cursor == 0 || cursor > value.len() {
        return None;
    }
    // The token starts right after the last whitespace char before the caret.
    let mut token_start = 0usize;
    for (i, ch) in value.char_indices() {
        if i >= cursor {
            break;
        }
        if ch.is_whitespace() {
            token_start = i + ch.len_utf8();
        }
    }
    let token = value.get(token_start..cursor)?;
    let mut chars = token.chars();
    let trigger = chars.next()?;
    if trigger != '/' && trigger != '@' {
        return None;
    }
    // A trigger token cannot itself contain whitespace (the scan already split
    // on it), so the remainder is the query verbatim.
    let query = chars.collect();
    Some(Detection {
        trigger,
        token_start,
        query,
    })
}

/// All registered slash commands, filtered + sorted by `query`.
pub fn slash_source(query: &str) -> Vec<CompletionItem> {
    let Some(reg) = SlashCommandRegistry::global() else {
        return Vec::new();
    };
    let items: Vec<CompletionItem> = reg
        .commands()
        .map(|cmd| CompletionItem {
            name: cmd.name().to_string().into(),
            description: cmd.description().to_string().into(),
            kind: CompletionKind::Command,
        })
        .collect();
    filter_sort(items, query)
}

/// Skills + subagents, filtered + sorted by `query`.
pub fn mention_source(query: &str) -> Vec<CompletionItem> {
    let mut items = Vec::new();
    for def in skill::global().list() {
        items.push(CompletionItem {
            name: def.name.clone().into(),
            description: def.description.clone().into(),
            kind: CompletionKind::Skill,
        });
    }
    for def in agent_def::global().list() {
        items.push(CompletionItem {
            name: def.def.name.clone().into(),
            description: def.def.description.clone().into(),
            kind: CompletionKind::Agent,
        });
    }
    filter_sort(items, query)
}

/// Prefix matches first, then substring matches, then alphabetical. An empty
/// query returns everything (prefix-matched trivially).
fn filter_sort(items: Vec<CompletionItem>, query: &str) -> Vec<CompletionItem> {
    let q = query.to_lowercase();
    if q.is_empty() {
        return items;
    }
    let mut matched: Vec<(bool, CompletionItem)> = items
        .into_iter()
        .filter_map(|it| {
            let name_l = it.name.to_lowercase();
            if name_l.starts_with(&q) {
                Some((true, it))
            } else if name_l.contains(&q) {
                Some((false, it))
            } else {
                None
            }
        })
        .collect();
    matched.sort_by(|(pa, a), (pb, b)| match (pa, pb) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    });
    matched.into_iter().map(|(_, it)| it).collect()
}

/// The live popover snapshot held by the workspace while open.
pub struct CompletionState {
    pub trigger: char,
    pub token_start: usize,
    pub items: Vec<CompletionItem>,
    pub selected: usize,
    pub list_state: ListState,
}

impl CompletionState {
    pub fn new(trigger: char, token_start: usize, items: Vec<CompletionItem>) -> Self {
        let list_state = ListState::new(items.len(), ListAlignment::Top, px(0.));
        if !items.is_empty() {
            list_state.scroll_to_reveal_item(0);
        }
        Self {
            trigger,
            token_start,
            items,
            selected: 0,
            list_state,
        }
    }

    /// Move the selection by `delta` (wrapping); scroll the new row into view.
    pub fn move_selection(&mut self, delta: i32) {
        if self.items.is_empty() {
            return;
        }
        let n = self.items.len() as i32;
        let mut next = self.selected as i32 + delta;
        next = ((next % n) + n) % n;
        self.selected = next as usize;
        self.list_state.scroll_to_reveal_item(self.selected);
    }
}

/// Build the replacement for confirm: `prefix + trigger+name+" " + suffix`,
/// returning the new value and the byte offset where the caret should land
/// (right after the trailing space).
pub fn build_replacement(
    trigger: char,
    name: &str,
    value: &str,
    token_start: usize,
    cursor: usize,
) -> (String, usize) {
    let prefix = &value[..token_start.min(value.len())];
    let suffix = &value[cursor.min(value.len())..];
    // Exactly one separator follows the inserted name; if the suffix already
    // begins with one, drop it so the two don't double up. The caret lands
    // after that separator so typing can continue seamlessly.
    let head = format!("{}{} ", trigger, name);
    let suffix = match suffix.chars().next() {
        Some(c) if c.is_whitespace() => &suffix[c.len_utf8()..],
        _ => suffix,
    };
    let caret = prefix.len() + head.len();
    (format!("{prefix}{head}{suffix}"), caret)
}

/// Callback invoked when a completion row is selected (by click or keyboard).
pub type SelectHandler = Rc<dyn Fn(usize, &mut Window, &mut App)>;

/// Render the popover list. `on_select(ix)` fires on click or keyboard confirm.
pub fn render_completion(
    state: &CompletionState,
    theme: &Theme,
    on_select: SelectHandler,
) -> AnyElement {
    let trigger = state.trigger;
    let items = state.items.clone();
    let selected = state.selected;
    let fg = theme.foreground;
    let muted = theme.muted_foreground;
    let secondary = theme.secondary;
    let border = theme.border;
    let radius = theme.radius;
    let mono = theme.mono_font_family.clone();
    let on_select = on_select.clone();

    let list = gpui::list(state.list_state.clone(), move |ix, _window, _cx| {
        let Some(item) = items.get(ix) else {
            return gpui::div().into_any_element();
        };
        let is_selected = ix == selected;
        let label = format!("{trigger}{}", item.name);
        let desc = if item.description.is_empty() {
            None
        } else {
            Some(item.description.clone())
        };
        let tag = i18n::t(item.kind.tag_key());
        let on_select = on_select.clone();
        let mono = mono.clone();
        let mut row = h_flex()
            .id(("completion-row", ix))
            .w_full()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .cursor_pointer()
            .child(Icon::new(item.kind.icon()).small().text_color(muted))
            .child(
                gpui::div()
                    .text_sm()
                    .text_color(fg)
                    .child(gpui::StyledText::new(label)),
            );
        if let Some(desc) = desc {
            row = row.child(
                gpui::div()
                    .flex_1()
                    .min_w_0()
                    .text_xs()
                    .text_color(muted)
                    .child(desc),
            );
        }
        row = row.child(gpui::div().text_xs().text_color(muted).child(tag.clone()));
        // Apply the mono family + selected highlight at the row root so the
        // whole line reads as code and the highlight spans full width.
        let mut row = row.font_family(mono);
        if is_selected {
            row = row.bg(secondary);
        }
        row.on_click(move |_, window, cx| on_select(ix, window, cx))
            .into_any_element()
    })
    .w_full()
    .max_h(px(300.))
    .min_w_0();

    v_flex()
        .w_full()
        .max_w(px(520.))
        .child(
            v_flex()
                .w_full()
                .bg(theme.background)
                .border_1()
                .border_color(border)
                .rounded(radius)
                .shadow_md()
                .child(list),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_slash_at_start() {
        let d = detect("/yo", 3).unwrap();
        assert_eq!(d.trigger, '/');
        assert_eq!(d.token_start, 0);
        assert_eq!(d.query, "yo");
    }

    #[test]
    fn detect_at_mid_text() {
        let d = detect("hello @gi", 9).unwrap();
        assert_eq!(d.trigger, '@');
        assert_eq!(d.token_start, 6);
        assert_eq!(d.query, "gi");
    }

    #[test]
    fn detect_none_after_space() {
        // Caret right after a space: the token is empty.
        assert!(detect("/yolo ", 6).is_none());
    }

    #[test]
    fn detect_none_without_trigger() {
        assert!(detect("hello world", 11).is_none());
    }

    #[test]
    fn detect_none_at_zero_cursor() {
        assert!(detect("/", 0).is_none());
    }

    #[test]
    fn filter_sort_prefix_first() {
        let items = vec![
            CompletionItem {
                name: "github".into(),
                description: "".into(),
                kind: CompletionKind::Skill,
            },
            CompletionItem {
                name: "git".into(),
                description: "".into(),
                kind: CompletionKind::Skill,
            },
        ];
        let out = filter_sort(items, "gi");
        assert_eq!(out[0].name, "git");
        assert_eq!(out[1].name, "github");
    }

    #[test]
    fn filter_sort_empty_query_keeps_all() {
        let items = vec![CompletionItem {
            name: "yolo".into(),
            description: "".into(),
            kind: CompletionKind::Command,
        }];
        assert_eq!(filter_sort(items, "").len(), 1);
    }

    #[test]
    fn build_replacement_preserves_suffix() {
        let (new, caret) = build_replacement('@', "github", "hello @gi rest", 6, 9);
        assert_eq!(new, "hello @github rest");
        assert_eq!(caret, "hello @github ".len());
    }
}
