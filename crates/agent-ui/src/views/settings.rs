//! Settings overlay — a single-window alternative to opening a separate
//! preferences window. Mounts inline over the Workspace via
//! `Workspace::view_mode`; clicks on sidebar items update a local
//! `selected` highlight and emit no further side effects yet.

use gpui::{
    Animation, AnimationExt as _, AnyElement, Context, Entity, EventEmitter, Render, SharedString,
    Window, ease_out_quint, prelude::*, px,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, h_flex,
    input::{Input, InputState},
    v_flex,
};

use agent::i18n;

const SIDEBAR_W: f32 = 260.;
/// How long the click-flash on a freshly-clicked item takes to fade from its
/// peak intensity back to the steady selected tint.
const CLICK_FLASH_MS: u64 = 280;

/// A single static settings item (icon + label key, optional trailing icon).
/// `label` is a fluent message id resolved via `i18n::t` at render time, so the
/// displayed text tracks the UI locale while the id itself stays a stable key
/// for selection / element-id purposes.
#[derive(Clone)]
struct SettingsItem {
    icon: IconName,
    label: &'static str,
    trailing: Option<IconName>,
}

/// A section group (title key + items).
struct SettingsGroup {
    title: &'static str,
    items: &'static [SettingsItem],
}

const GROUPS: &[SettingsGroup] = &[
    SettingsGroup {
        title: "settings-group-general",
        items: &[
            SettingsItem::new(IconName::Settings, "settings-item-general", None),
            SettingsItem::new(IconName::Sun, "settings-item-appearance", None),
            SettingsItem::new(IconName::Cpu, "settings-item-config", None),
            SettingsItem::new(IconName::Star, "settings-item-personalization", None),
            SettingsItem::new(IconName::Heart, "settings-item-pets", None),
            SettingsItem::new(IconName::Frame, "settings-item-keyboard", None),
        ],
    },
    SettingsGroup {
        title: "settings-group-integrations",
        items: &[
            SettingsItem::new(IconName::Bot, "settings-item-snapshots", None),
            SettingsItem::new(IconName::ChartPie, "settings-item-mcp", None),
            SettingsItem::new(IconName::Globe, "settings-item-browser", None),
            SettingsItem::new(IconName::Ellipsis, "settings-item-computer", None),
        ],
    },
    SettingsGroup {
        title: "settings-group-coding",
        items: &[
            SettingsItem::new(IconName::Asterisk, "settings-item-hooks", None),
            SettingsItem::new(IconName::Ellipsis, "settings-item-connections", None),
            SettingsItem::new(IconName::Github, "settings-item-git", None),
            SettingsItem::new(IconName::Folder, "settings-item-environment", None),
            SettingsItem::new(IconName::FolderOpen, "settings-item-worktrees", None),
        ],
    },
    SettingsGroup {
        title: "settings-group-archived",
        items: &[
            SettingsItem::new(IconName::Inbox, "settings-item-archived", None),
            SettingsItem::new(
                IconName::Map,
                "settings-item-chat-settings",
                Some(IconName::ExternalLink),
            ),
        ],
    },
];

impl SettingsItem {
    const fn new(icon: IconName, label: &'static str, trailing: Option<IconName>) -> Self {
        Self {
            icon,
            label,
            trailing,
        }
    }
}

pub struct SettingsView {
    search: Entity<InputState>,
    /// The item the user last clicked. Static panels wire no further behavior
    /// yet; the field exists so the row highlight + click affordance match the
    /// production feel.
    selected: Option<SharedString>,
    /// Monotonic counter that bumps on every click. The render path embeds
    /// this into each item's click-pulse animation id so a fresh tween fires
    /// on every click (a stable id would only animate once).
    click_gen: u64,
}

/// Events emitted from the Settings view to its host (the Workspace).
/// The host subscribes to decide what to do on each.
#[derive(Clone)]
pub enum SettingsEvent {
    /// The user clicked "Back to app"; the host should switch back to the
    /// workspace view.
    Exit,
}

impl EventEmitter<SettingsEvent> for SettingsView {}

impl SettingsView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search =
            cx.new(|cx| InputState::new(window, cx).placeholder(i18n::t("settings-search-placeholder")));
        Self {
            search,
            selected: None,
            click_gen: 0,
        }
    }
}

impl Render for SettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let selected = self.selected.clone();
        let search = self.search.clone();

        // Back-row click handler requests the host to exit Settings mode.
        let on_back = cx.listener(|_this, _ev, _window, cx| {
            cx.emit(SettingsEvent::Exit);
        });

        // Build group columns. Each item row is registered inline so the click
        // listener is created in the same context as the parent cx (avoids any
        // cross-helper borrow/lifetime surprises).
        let mut groups: Vec<AnyElement> = Vec::with_capacity(GROUPS.len());
        for group in GROUPS.iter() {
            let title = i18n::t(group.title);
            let mut column = v_flex().gap_0p5().child(
                gpui::div()
                    .px_3()
                    .pt_2()
                    .pb_1()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(title),
            );
            for it in group.items.iter() {
                // `it.label` is a fluent message id; the resolved display text
                // is what the user sees, while the id is the stable selection
                // key and element id.
                let label_str: SharedString = i18n::t(it.label);
                let is_selected = selected.as_ref().map(|s| s.as_str()) == Some(it.label);
                let bg = if is_selected {
                    theme.accent.opacity(0.12)
                } else {
                    theme.transparent
                };
                let icon = it.icon.clone();
                let trailing = it.trailing.clone();
                let label_key = it.label;
                // Bump the per-view click generation on every click. The
                // render path embeds this into each item's pulse animation
                // id, so a fresh tween fires each time the user clicks.
                let on_click = cx.listener(move |this, _ev, _window, cx| {
                    this.selected = Some(label_key.into());
                    this.click_gen = this.click_gen.wrapping_add(1);
                    cx.notify();
                });
                let mut row = h_flex()
                    .id(it.label)
                    .w_full()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_1p5()
                    .rounded(theme.radius)
                    .text_sm()
                    .text_color(theme.foreground)
                    .bg(bg)
                    .hover(|s| s.bg(theme.accent.opacity(0.08)))
                    // Pressed state: while the mouse button is held, deepen
                    // the row bg so the user gets a visible click press.
                    .active(|s| s.bg(theme.accent.opacity(0.24)))
                    .cursor_pointer()
                    .on_click(on_click)
                    .child(Icon::new(icon).small().text_color(theme.muted_foreground))
                    .child(gpui::div().flex_1().min_w_0().child(label_str.clone()));
                if let Some(t) = trailing {
                    row = row.child(Icon::new(t).small().text_color(theme.muted_foreground));
                }
                // Click pulse: the freshly-clicked row briefly tints deeper
                // than the steady selected bg, then fades back. The id mixes
                // the item label and the current click generation so a new
                // click always restarts the tween.
                if is_selected {
                    let anim_id = format!("settings-click-pulse-{}", self.click_gen);
                    let pulse_el = gpui::div()
                        .size_full()
                        .absolute()
                        .bg(theme.accent.opacity(0.30))
                        .rounded(theme.radius)
                        .with_animation(
                            anim_id,
                            Animation::new(std::time::Duration::from_millis(CLICK_FLASH_MS))
                                .with_easing(ease_out_quint()),
                            move |el, delta| el.opacity(1.0 - delta),
                        );
                    row = row.child(pulse_el);
                }
                column = column.child(row);
            }
            groups.push(column.into_any_element());
        }

        h_flex()
            .w_full()
            .h_full()
            .bg(theme.background)
            // Left pane: mirror of the in-app sidebar (same width, same background).
            .child(
                v_flex()
                    .h_full()
                    .w(px(SIDEBAR_W))
                    .bg(theme.background)
                    .border_r_1()
                    .border_color(theme.border)
                    .child(
                        v_flex()
                            .w_full()
                            .p_2()
                            .gap_1()
                            .child(
                                h_flex()
                                    .id("settings-back")
                                    .items_center()
                                    .gap_2()
                                    .px_2()
                                    .py_1p5()
                                    .rounded(theme.radius)
                                    .text_sm()
                                    .text_color(theme.foreground)
                                    .hover(|s| s.bg(theme.accent.opacity(0.08)))
                                    .cursor_pointer()
                                    .on_click(on_back)
                                    .child(Icon::new(IconName::ArrowLeft).small())
                                    .child(i18n::t("settings-back")),
                            )
                            .child(
                                h_flex()
                                    .w_full()
                                    .items_center()
                                    .gap_2()
                                    .px_2()
                                    .py_1()
                                    .rounded(theme.radius)
                                    .bg(theme.secondary)
                                    .child(
                                        Icon::new(IconName::Search)
                                            .small()
                                            .text_color(theme.muted_foreground),
                                    )
                                    .child(
                                        Input::new(&search)
                                            .appearance(false)
                                            .bordered(false)
                                            .focus_bordered(false),
                                    ),
                            ),
                    )
                    .child(
                        v_flex()
                            .id("settings-groups")
                            .flex_1()
                            .overflow_y_scroll()
                            .px_2()
                            .pb_2()
                            .gap_3()
                            .children(groups),
                    ),
            )
            // Right pane: centered "Coming soon…" placeholder, suffixed with
            // the currently selected item so clicks have visible feedback.
            .child({
                let coming_label: SharedString = match selected.as_ref() {
                    Some(key) => {
                        let displayed = i18n::t(key);
                        i18n::t_str("settings-coming-soon-label", &[("label", displayed.as_ref())])
                    }
                    None => i18n::t("settings-coming-soon"),
                };
                h_flex()
                    .flex_1()
                    .h_full()
                    .items_center()
                    .justify_center()
                    .child(
                        gpui::div()
                            .text_xl()
                            .text_color(theme.muted_foreground)
                            .child(coming_label),
                    )
            })
    }
}
