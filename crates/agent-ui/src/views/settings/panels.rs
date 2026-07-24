//! Right-pane renderers for the Settings overlay. Each `render_*_panel` is a
//! free function that takes `&mut SettingsView` so the closures it builds for
//! the various controls can update view state directly through the
//! `Entity<Self>` handle captured at render time.
//!
//! Visual style: a vertical stack of section
//! cards. Each card is a rounded rectangle with a subtle secondary background
//! and a single-column list of rows. A row is title (left) + control (right),
//! with an optional muted description line below the title. Rows are separated
//! by a single hairline border so the card reads as a list rather than a
//! grid of unrelated items.

use std::sync::Arc;

use gpui::{
    Anchor, AnyElement, Context, Entity, Hsla, InteractiveElement, IntoElement, ParentElement as _,
    SharedString, StatefulInteractiveElement, Styled as _, div, prelude::FluentBuilder as _, px,
};
use gpui_component::theme::Theme;
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants},
    h_flex,
    menu::{DropdownMenu, PopupMenuItem},
    notification::Notification,
    switch::Switch,
    v_flex,
};

use agent::i18n;
use agent::mcp;

use super::{MOCK_PROJECTS, MockProject, SettingsView, WorkMode};

// --- Layout helpers -------------------------------------------------------

/// Wraps the panel content in a vertical scroll container that fills the
/// right pane. The scrollbar appears automatically; we don't need to track
/// the scroll position across re-renders for a settings overlay.
fn panel_scroll(content: impl IntoElement) -> AnyElement {
    v_flex()
        .flex_1()
        .h_full()
        .min_h_0()
        .min_w_0()
        .id("settings-right-pane")
        .overflow_y_scroll()
        .p_4()
        .gap_4()
        .child(content)
        .into_any_element()
}

/// A single settings row: title (left) + control (right), with an optional
/// muted description. The description slot is an `AnyElement` so callers can
/// include link chips or other inline widgets without losing styling.
fn row_with_control(
    title: SharedString,
    description: Option<AnyElement>,
    control: AnyElement,
) -> AnyElement {
    h_flex()
        .w_full()
        .items_start()
        .justify_between()
        .gap_3()
        .px_3()
        .py_3()
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_1()
                .child(
                    div()
                        .text_sm()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .truncate()
                        .child(title),
                )
                .when_some(description, |this, desc| {
                    this.child(
                        div()
                            .text_xs()
                            .text_color(gpui::transparent_black())
                            .child(desc),
                    )
                }),
        )
        .child(control)
        .into_any_element()
}

/// A row used as a section header: title only, no control, smaller type.
fn section_header(label: &'static str) -> AnyElement {
    div()
        .px_3()
        .pt_3()
        .pb_1()
        .text_xs()
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(gpui::transparent_black())
        .child(i18n::t(label))
        .into_any_element()
}

fn hairline(divider: Hsla) -> AnyElement {
    div()
        .h(px(1.))
        .w_full()
        .bg(divider)
        .mx_3()
        .into_any_element()
}

/// Section card: rounded rectangle with a secondary background that hosts a
/// list of pre-built rows. A single hairline divider is injected between
/// adjacent rows (not after the last one) so the card reads as a list.
fn section_card(theme: &Theme, children: Vec<AnyElement>) -> AnyElement {
    let bg = theme.secondary.opacity(0.45);
    let divider = theme.border.opacity(0.6);
    let muted = theme.muted_foreground;
    let mut col = v_flex().w_full().p_2().gap_0().rounded(px(10.)).bg(bg);
    let last_ix = children.len().saturating_sub(1);
    for (ix, child) in children.into_iter().enumerate() {
        if ix == 0 {
            // First child is the section header — paint muted-foreground text.
            col = col.child(
                div()
                    .text_xs()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(muted)
                    .child(child),
            );
        } else {
            col = col.child(child);
        }
        if ix < last_ix {
            col = col.child(div().h(px(1.)).w_full().bg(divider).mx_3());
        }
    }
    col.into_any_element()
}

fn muted_text(label: SharedString, muted: Hsla) -> AnyElement {
    div()
        .text_xs()
        .text_color(muted)
        .child(label)
        .into_any_element()
}

// --- Common controls ------------------------------------------------------

type BoolApply = Arc<dyn Fn(&mut SettingsView, bool) + Send + Sync + 'static>;
type StringApply = Arc<dyn Fn(&mut SettingsView, SharedString) + Send + Sync + 'static>;
/// Persists a pick. The display value must flip only on a successful save, so
/// the whole save-then-apply sequence lives in one closure that returns the
/// outcome — the caller (which has the window) surfaces a failure as a toast.
type PostApply = Arc<
    dyn Fn(&mut SettingsView, &SharedString, &mut Context<SettingsView>) -> Result<(), String>
        + Send
        + Sync
        + 'static,
>;

/// Build a Switch bound to a field via the view entity. The `apply` closure
/// runs inside `Entity::update`, so it can mutate view state freely.
fn mock_switch(
    id: impl Into<gpui::ElementId>,
    checked: bool,
    view: Entity<SettingsView>,
    apply: BoolApply,
) -> AnyElement {
    Switch::new(id)
        .checked(checked)
        .on_click(move |&new_val, _window, cx| {
            view.update(cx, |this, cx| {
                apply(this, new_val);
                cx.notify();
            });
        })
        .into_any_element()
}

/// Build a dropdown Button that updates a `SharedString` field on the view.
fn mock_dropdown(
    id: impl Into<gpui::ElementId>,
    label: SharedString,
    options: Vec<SharedString>,
    view: Entity<SettingsView>,
    apply: StringApply,
) -> AnyElement {
    Button::new(id)
        .label(label)
        .dropdown_caret(true)
        .outline()
        .dropdown_menu_with_anchor(Anchor::BottomRight, move |menu, _window, _cx| {
            options.iter().fold(menu, |menu, opt| {
                let opt = opt.clone();
                let view = view.clone();
                let apply = apply.clone();
                menu.item(
                    PopupMenuItem::new(opt.clone()).on_click(move |_ev, _window, cx| {
                        view.update(cx, |this, cx| {
                            apply(this, opt.clone());
                            cx.notify();
                        });
                    }),
                )
            })
        })
        .into_any_element()
}

/// A dropdown whose pick persists via `post`. The display value flips only on
/// a successful save (enforced inside `post`); a save failure is surfaced as an
/// in-app toast here, where the window is in scope.
fn mock_dropdown_with_post(
    id: impl Into<gpui::ElementId>,
    label: SharedString,
    options: Vec<(SharedString, SharedString)>,
    view: Entity<SettingsView>,
    post: PostApply,
) -> AnyElement {
    Button::new(id)
        .label(label)
        .dropdown_caret(true)
        .outline()
        .dropdown_menu_with_anchor(Anchor::BottomRight, move |menu, _window, _cx| {
            options.iter().fold(menu, |menu, (display, token)| {
                let display = display.clone();
                let token = token.clone();
                let view = view.clone();
                let post = post.clone();
                menu.item(
                    PopupMenuItem::new(display.clone()).on_click(move |_ev, window, cx| {
                        if let Err(msg) = view.update(cx, |this, cx| post(this, &token, cx))
                            && !msg.is_empty()
                        {
                            window.push_notification(
                                Notification::error(msg)
                                    .title(i18n::t("settings-save-failed-title")),
                                cx,
                            );
                        }
                    }),
                )
            })
        })
        .into_any_element()
}

/// Segmented 2-button group. Both segments are styled identically; the
/// active one is filled, the inactive one outlined.
fn mock_segmented(
    id: impl Into<gpui::ElementId>,
    active: bool,
    label: SharedString,
    view: Entity<SettingsView>,
    apply: Arc<dyn Fn(&mut SettingsView) + Send + Sync + 'static>,
) -> AnyElement {
    let view = view;
    let id = id.into();
    let label_for_active = label.clone();
    let label_for_inactive = label;
    if active {
        Button::new(id)
            .label(label_for_active)
            .small()
            .on_click(move |_ev, _window, cx| {
                view.update(cx, |this, cx| {
                    apply(this);
                    cx.notify();
                });
            })
            .into_any_element()
    } else {
        Button::new(id)
            .label(label_for_inactive)
            .small()
            .outline()
            .on_click(move |_ev, _window, cx| {
                view.update(cx, |this, cx| {
                    apply(this);
                    cx.notify();
                });
            })
            .into_any_element()
    }
}

fn build_segmented_pair(
    id_prefix: &'static str,
    active_is_left: bool,
    left_label: SharedString,
    right_label: SharedString,
    view: Entity<SettingsView>,
    pick_left: Arc<dyn Fn(&mut SettingsView) + Send + Sync + 'static>,
    pick_right: Arc<dyn Fn(&mut SettingsView) + Send + Sync + 'static>,
) -> AnyElement {
    h_flex()
        .gap_2()
        .child(mock_segmented(
            format!("{}-L", id_prefix),
            active_is_left,
            left_label,
            view.clone(),
            pick_left,
        ))
        .child(mock_segmented(
            format!("{}-R", id_prefix),
            !active_is_left,
            right_label,
            view,
            pick_right,
        ))
        .into_any_element()
}

// --- General panel --------------------------------------------------------

pub fn render_general(view: &mut SettingsView, cx: &mut Context<SettingsView>) -> AnyElement {
    let theme = cx.theme().clone();
    let entity = cx.entity();
    let muted = theme.muted_foreground;

    // --- Work mode section: 2-card selector ---
    let work_mode_section = {
        let entity = entity.clone();
        let card_bg_active = theme.accent.opacity(0.12);
        let border_active = theme.accent;
        let border_idle = theme.border;
        let card_for = |mode: WorkMode,
                        title: SharedString,
                        desc: SharedString,
                        icon: IconName,
                        active: bool| {
            let entity = entity.clone();
            let border_color = if active { border_active } else { border_idle };
            let card_bg = if active {
                card_bg_active
            } else {
                gpui::transparent_black()
            };
            let icon_color = if active { theme.foreground } else { muted };
            h_flex()
                .flex_1()
                .min_w_0()
                .items_center()
                .gap_3()
                .p_3()
                .rounded(px(8.))
                .border_1()
                .border_color(border_color)
                .bg(card_bg)
                .cursor_pointer()
                .id("wm-card")
                .on_click(move |_ev, _window, cx| {
                    entity.update(cx, |this, cx| {
                        this.work_mode = mode;
                        cx.notify();
                    });
                })
                .child(Icon::new(icon).small().text_color(icon_color))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_0p5()
                        .child(
                            div()
                                .text_sm()
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .truncate()
                                .child(title),
                        )
                        .child(div().text_xs().text_color(muted).child(desc)),
                )
                .into_any_element()
        };
        let programming = card_for(
            WorkMode::Programming,
            i18n::t("settings-row-work-mode-programming"),
            i18n::t("settings-desc-work-mode-programming"),
            IconName::SquareTerminal,
            view.work_mode == WorkMode::Programming,
        );
        let workday = card_for(
            WorkMode::Workday,
            i18n::t("settings-row-work-mode-workday"),
            i18n::t("settings-desc-work-mode-workday"),
            IconName::Globe,
            view.work_mode == WorkMode::Workday,
        );
        let children = vec![
            section_header("settings-section-work-mode"),
            row_with_control(
                i18n::t("settings-desc-work-mode"),
                None,
                h_flex()
                    .gap_3()
                    .child(programming)
                    .child(workday)
                    .into_any_element(),
            ),
        ];
        section_card(&theme, children)
    };

    // --- Permissions section ---
    let permissions_section = {
        let entity = entity.clone();
        let learn_more: SharedString = i18n::t("settings-link-learn-more");
        let build_desc = |label: SharedString, learn: SharedString| -> AnyElement {
            h_flex()
                .items_center()
                .gap_1()
                .child(muted_text(label, muted))
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.link)
                        .cursor_pointer()
                        .id("link-learn-more")
                        .on_click(|_ev, _window, _cx| {
                            tracing::info!("learn more clicked (no-op in this build)");
                        })
                        .child(learn),
                )
                .into_any_element()
        };
        let desc_auto = build_desc(
            i18n::t("settings-desc-permission-autopilot"),
            learn_more.clone(),
        );
        let desc_danger = build_desc(i18n::t("settings-desc-permission-danger"), learn_more);
        let children = vec![
            section_header("settings-section-permissions"),
            row_with_control(
                i18n::t("settings-row-permission-autopilot"),
                Some(desc_auto),
                mock_switch(
                    "perm-autopilot",
                    view.permission_autopilot,
                    entity.clone(),
                    Arc::new(|this, v| this.permission_autopilot = v),
                ),
            ),
            hairline(theme.border.opacity(0.6)),
            row_with_control(
                i18n::t("settings-row-permission-danger"),
                Some(desc_danger),
                mock_switch(
                    "perm-danger",
                    view.permission_danger,
                    entity,
                    Arc::new(|this, v| this.permission_danger = v),
                ),
            ),
        ];
        section_card(&theme, children)
    };

    // --- General (misc) section ---
    let general_section = {
        let entity = entity.clone();
        let term_bottom_label = i18n::t("settings-value-bottom");
        let term_right_label = i18n::t("settings-value-right");
        let cr_inline_label = i18n::t("settings-value-inline");
        let cr_detached_label = i18n::t("settings-value-detached");
        let terminal_active_is_left = view.terminal_location == term_bottom_label;
        let code_review_active_is_left = view.code_review_mode == cr_inline_label;

        let children = vec![
            section_header("settings-section-general-misc"),
            row_with_control(
                i18n::t("settings-row-file-target"),
                Some(muted_text(i18n::t("settings-desc-file-target"), muted)),
                mock_dropdown(
                    "file-target",
                    view.file_target.clone(),
                    vec![i18n::t("settings-value-vscode")],
                    entity.clone(),
                    Arc::new(|this, v| this.file_target = v),
                ),
            ),
            hairline(theme.border.opacity(0.6)),
            row_with_control(
                i18n::t("settings-row-ui-language"),
                Some(muted_text(i18n::t("settings-desc-ui-language"), muted)),
                mock_dropdown_with_post(
                    "ui-language",
                    view.ui_language.clone(),
                    vec![
                        (SharedString::from("English"), SharedString::from("en")),
                        (SharedString::from("简体中文"), SharedString::from("zh-CN")),
                    ],
                    entity.clone(),
                    Arc::new(|this, value, cx| this.persist_ui_language(value.clone(), cx)),
                ),
            ),
            hairline(theme.border.opacity(0.6)),
            row_with_control(
                i18n::t("settings-row-agent-language"),
                Some(muted_text(i18n::t("settings-desc-agent-language"), muted)),
                mock_dropdown_with_post(
                    "agent-language",
                    view.agent_language.clone(),
                    vec![
                        (SharedString::from("English"), SharedString::from("en")),
                        (SharedString::from("简体中文"), SharedString::from("zh-CN")),
                    ],
                    entity.clone(),
                    Arc::new(|this, value, cx| this.persist_agent_language(value.clone(), cx)),
                ),
            ),
            hairline(theme.border.opacity(0.6)),
            row_with_control(
                i18n::t("settings-row-menu-bar"),
                Some(muted_text(i18n::t("settings-desc-menu-bar"), muted)),
                mock_switch(
                    "menu-bar",
                    view.show_in_menu_bar,
                    entity.clone(),
                    Arc::new(|this, v| this.show_in_menu_bar = v),
                ),
            ),
            hairline(theme.border.opacity(0.6)),
            row_with_control(
                i18n::t("settings-row-bottom-panel"),
                Some(muted_text(i18n::t("settings-desc-bottom-panel"), muted)),
                mock_switch(
                    "bottom-panel",
                    view.bottom_panel,
                    entity.clone(),
                    Arc::new(|this, v| this.bottom_panel = v),
                ),
            ),
            hairline(theme.border.opacity(0.6)),
            row_with_control(
                i18n::t("settings-row-terminal-location"),
                Some(muted_text(
                    i18n::t("settings-desc-terminal-location"),
                    muted,
                )),
                build_segmented_pair(
                    "term",
                    terminal_active_is_left,
                    term_bottom_label.clone(),
                    term_right_label.clone(),
                    entity.clone(),
                    Arc::new(move |this| this.terminal_location = term_bottom_label.clone()),
                    Arc::new(move |this| this.terminal_location = term_right_label.clone()),
                ),
            ),
            hairline(theme.border.opacity(0.6)),
            row_with_control(
                i18n::t("settings-row-keep-awake"),
                Some(muted_text(i18n::t("settings-desc-keep-awake"), muted)),
                mock_switch(
                    "keep-awake",
                    view.keep_awake,
                    entity.clone(),
                    Arc::new(|this, v| this.keep_awake = v),
                ),
            ),
            hairline(theme.border.opacity(0.6)),
            row_with_control(
                i18n::t("settings-row-code-review"),
                Some(muted_text(i18n::t("settings-desc-code-review"), muted)),
                build_segmented_pair(
                    "cr",
                    code_review_active_is_left,
                    cr_inline_label.clone(),
                    cr_detached_label.clone(),
                    entity.clone(),
                    Arc::new(move |this| this.code_review_mode = cr_inline_label.clone()),
                    Arc::new(move |this| this.code_review_mode = cr_detached_label.clone()),
                ),
            ),
            hairline(theme.border.opacity(0.6)),
            row_with_control(
                i18n::t("settings-row-import"),
                Some(muted_text(i18n::t("settings-desc-import"), muted)),
                Button::new("import")
                    .label(i18n::t("settings-btn-import"))
                    .outline()
                    .on_click(|_ev, _window, _cx| {
                        tracing::info!("import clicked (no-op in this build)");
                    })
                    .into_any_element(),
            ),
            hairline(theme.border.opacity(0.6)),
            row_with_control(
                i18n::t("settings-row-licenses"),
                Some(muted_text(i18n::t("settings-desc-licenses"), muted)),
                Button::new("licenses")
                    .label(i18n::t("settings-btn-view"))
                    .outline()
                    .on_click(|_ev, _window, _cx| {
                        tracing::info!("view licenses clicked (no-op in this build)");
                    })
                    .into_any_element(),
            ),
        ];
        section_card(&theme, children)
    };

    // --- Editor section ---
    let editor_section = {
        let children = vec![
            section_header("settings-section-editor"),
            row_with_control(
                i18n::t("settings-row-send-shortcut"),
                Some(muted_text(i18n::t("settings-desc-send-shortcut"), muted)),
                mock_dropdown(
                    "send-shortcut",
                    view.send_shortcut.clone(),
                    vec![i18n::t("settings-value-enter-shift")],
                    entity.clone(),
                    Arc::new(|this, v| this.send_shortcut = v),
                ),
            ),
        ];
        section_card(&theme, children)
    };

    // --- Pop-up section ---
    let popup_section = {
        let entity = entity.clone();
        let popup_set = entity.clone();
        let children = vec![
            section_header("settings-section-pop-up"),
            row_with_control(
                i18n::t("settings-row-pop-up-shortcut"),
                Some(muted_text(i18n::t("settings-desc-pop-up-shortcut"), muted)),
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(muted_text(view.pop_up_shortcut_status.clone(), muted))
                    .child(
                        Button::new("popup-set")
                            .label(i18n::t("settings-btn-set"))
                            .outline()
                            .on_click(move |_ev, _window, cx| {
                                popup_set.update(cx, |this, cx| {
                                    this.pop_up_shortcut_status =
                                        i18n::t("settings-value-configured");
                                    cx.notify();
                                });
                            })
                            .into_any_element(),
                    )
                    .into_any_element(),
            ),
            hairline(theme.border.opacity(0.6)),
            row_with_control(
                i18n::t("settings-row-default-no-project"),
                Some(muted_text(
                    i18n::t("settings-desc-default-no-project"),
                    muted,
                )),
                mock_switch(
                    "no-project",
                    view.default_no_project_chat,
                    entity,
                    Arc::new(|this, v| this.default_no_project_chat = v),
                ),
            ),
        ];
        section_card(&theme, children)
    };

    // --- Dictation section ---
    let dictation_section = {
        let entity = entity.clone();
        let press_set = entity.clone();
        let toggle_set = entity.clone();
        let children = vec![
            section_header("settings-section-dictation"),
            row_with_control(
                i18n::t("settings-row-microphone"),
                Some(muted_text(i18n::t("settings-desc-microphone"), muted)),
                mock_dropdown(
                    "microphone",
                    view.microphone.clone(),
                    vec![i18n::t("settings-value-system-default")],
                    entity.clone(),
                    Arc::new(|this, v| this.microphone = v),
                ),
            ),
            hairline(theme.border.opacity(0.6)),
            row_with_control(
                i18n::t("settings-row-press-dictate"),
                Some(muted_text(i18n::t("settings-desc-press-dictate"), muted)),
                Button::new("press-dictate-set")
                    .label(i18n::t("settings-btn-set"))
                    .outline()
                    .on_click(move |_ev, _window, cx| {
                        press_set.update(cx, |this, cx| {
                            this.press_dictate_status = i18n::t("settings-value-on");
                            cx.notify();
                        });
                    })
                    .into_any_element(),
            ),
            hairline(theme.border.opacity(0.6)),
            row_with_control(
                i18n::t("settings-row-toggle-dictate"),
                Some(muted_text(i18n::t("settings-desc-toggle-dictate"), muted)),
                Button::new("toggle-dictate-set")
                    .label(i18n::t("settings-btn-set"))
                    .outline()
                    .on_click(move |_ev, _window, cx| {
                        toggle_set.update(cx, |this, cx| {
                            this.toggle_dictate_status = i18n::t("settings-value-on");
                            cx.notify();
                        });
                    })
                    .into_any_element(),
            ),
            hairline(theme.border.opacity(0.6)),
            row_with_control(
                i18n::t("settings-row-keep-dictation-bar"),
                Some(muted_text(
                    i18n::t("settings-desc-keep-dictation-bar"),
                    muted,
                )),
                mock_switch(
                    "keep-dictation-bar",
                    view.keep_dictation_bar,
                    entity,
                    Arc::new(|this, v| this.keep_dictation_bar = v),
                ),
            ),
        ];
        section_card(&theme, children)
    };

    // --- Notifications section ---
    let notifications_section = {
        let entity = entity.clone();
        let children = vec![
            section_header("settings-section-notifications"),
            row_with_control(
                i18n::t("settings-row-turn-completion"),
                Some(muted_text(i18n::t("settings-desc-turn-completion"), muted)),
                mock_dropdown(
                    "turn-completion",
                    view.turn_completion_notify.clone(),
                    vec![
                        i18n::t("settings-value-focus-only"),
                        i18n::t("settings-value-off"),
                    ],
                    entity.clone(),
                    Arc::new(|this, v| this.turn_completion_notify = v),
                ),
            ),
            hairline(theme.border.opacity(0.6)),
            row_with_control(
                i18n::t("settings-row-permission-notify"),
                Some(muted_text(
                    i18n::t("settings-desc-permission-notify"),
                    muted,
                )),
                mock_switch(
                    "permission-notify",
                    view.permission_notify,
                    entity.clone(),
                    Arc::new(|this, v| this.permission_notify = v),
                ),
            ),
            hairline(theme.border.opacity(0.6)),
            row_with_control(
                i18n::t("settings-row-question-notify"),
                Some(muted_text(i18n::t("settings-desc-question-notify"), muted)),
                mock_switch(
                    "question-notify",
                    view.question_notify,
                    entity,
                    Arc::new(|this, v| this.question_notify = v),
                ),
            ),
        ];
        section_card(&theme, children)
    };

    panel_scroll(
        v_flex()
            .w_full()
            .gap_4()
            .child(work_mode_section)
            .child(permissions_section)
            .child(general_section)
            .child(editor_section)
            .child(popup_section)
            .child(dictation_section)
            .child(notifications_section),
    )
}

// --- Config panel ---------------------------------------------------------

pub fn render_config(view: &mut SettingsView, cx: &mut Context<SettingsView>) -> AnyElement {
    let theme = cx.theme().clone();
    let entity = cx.entity();
    let muted = theme.muted_foreground;

    let top_header = v_flex()
        .gap_1()
        .child(
            h_flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .text_base()
                        .font_weight(gpui::FontWeight::BLACK)
                        .child(i18n::t("settings-panel-config")),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.link)
                        .cursor_pointer()
                        .id("link-learn-more")
                        .on_click(|_ev, _window, _cx| {
                            tracing::info!("learn more clicked (no-op in this build)");
                        })
                        .child(i18n::t("settings-link-learn-more")),
                )
                .into_any_element(),
        )
        .child(muted_text(i18n::t("settings-desc-config-top"), muted))
        .into_any_element();

    let toml_section = {
        let entity = entity.clone();
        let children = vec![
            section_header("settings-section-config-toml"),
            row_with_control(
                i18n::t("settings-row-config-user"),
                None,
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(mock_dropdown(
                        "config-user",
                        view.config_user_target.clone(),
                        vec![i18n::t("settings-value-on")],
                        entity.clone(),
                        Arc::new(|this, v| this.config_user_target = v),
                    ))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.link)
                            .cursor_pointer()
                            .id("link-open-config")
                            .on_click(|_ev, _window, _cx| {
                                tracing::info!("open config.toml clicked (no-op)");
                            })
                            .child(i18n::t("settings-link-open-config")),
                    )
                    .into_any_element(),
            ),
            hairline(theme.border.opacity(0.6)),
            row_with_control(
                i18n::t("settings-row-config-approval"),
                Some(muted_text(i18n::t("settings-desc-config-approval"), muted)),
                mock_dropdown(
                    "config-approval",
                    view.config_approval_policy.clone(),
                    vec![i18n::t("settings-value-on-request")],
                    entity.clone(),
                    Arc::new(|this, v| this.config_approval_policy = v),
                ),
            ),
            hairline(theme.border.opacity(0.6)),
            row_with_control(
                i18n::t("settings-row-config-sandbox"),
                Some(muted_text(i18n::t("settings-desc-config-sandbox"), muted)),
                mock_dropdown(
                    "config-sandbox",
                    view.config_sandbox.clone(),
                    vec![i18n::t("settings-value-read-only")],
                    entity,
                    Arc::new(|this, v| this.config_sandbox = v),
                ),
            ),
        ];
        section_card(&theme, children)
    };

    let deps_section = {
        let entity = entity.clone();
        let children = vec![
            section_header("settings-section-workspace-deps"),
            row_with_control(
                i18n::t("settings-row-config-version"),
                None,
                // Build identifier captured at compile time — commit SHA and
                // build type. Not routed through i18n.
                muted_text(
                    SharedString::from(agent::version::full_version_string()),
                    muted,
                ),
            ),
            hairline(theme.border.opacity(0.6)),
            row_with_control(
                i18n::t("settings-row-config-builtin-deps"),
                Some(muted_text(
                    i18n::t("settings-desc-config-builtin-deps"),
                    muted,
                )),
                mock_switch(
                    "builtin-deps",
                    view.config_builtin_deps,
                    entity.clone(),
                    Arc::new(|this, v| this.config_builtin_deps = v),
                ),
            ),
            hairline(theme.border.opacity(0.6)),
            row_with_control(
                i18n::t("settings-btn-diagnose"),
                Some(muted_text(i18n::t("settings-desc-config-diagnose"), muted)),
                Button::new("diagnose")
                    .label(i18n::t("settings-btn-diagnose"))
                    .outline()
                    .on_click(|_ev, _window, _cx| {
                        tracing::info!("diagnose clicked (no-op in this build)");
                    })
                    .into_any_element(),
            ),
            hairline(theme.border.opacity(0.6)),
            row_with_control(
                i18n::t("settings-row-config-reinstall"),
                Some(muted_text(i18n::t("settings-desc-config-reinstall"), muted)),
                Button::new("reinstall")
                    .label(i18n::t("settings-btn-reinstall"))
                    .danger()
                    .on_click(|_ev, _window, _cx| {
                        tracing::info!("reinstall clicked (no-op in this build)");
                    })
                    .into_any_element(),
            ),
        ];
        section_card(&theme, children)
    };

    panel_scroll(
        v_flex()
            .w_full()
            .gap_4()
            .child(top_header)
            .child(toml_section)
            .child(deps_section),
    )
}

// --- Personalization panel ------------------------------------------------

pub fn render_personalization(
    view: &mut SettingsView,
    cx: &mut Context<SettingsView>,
) -> AnyElement {
    let theme = cx.theme().clone();
    let entity = cx.entity();
    let muted = theme.muted_foreground;

    let personality_section = {
        let entity = entity.clone();
        let children = vec![
            section_header("settings-section-personality"),
            row_with_control(
                i18n::t("settings-row-personality"),
                Some(muted_text(i18n::t("settings-desc-personality"), muted)),
                mock_dropdown(
                    "personality",
                    view.personality.clone(),
                    vec![i18n::t("settings-value-friendly")],
                    entity,
                    Arc::new(|this, v| this.personality = v),
                ),
            ),
        ];
        section_card(&theme, children)
    };

    let memory_section = {
        let entity = entity.clone();
        let children = vec![
            section_header_with_tag("settings-section-memory", "settings-tag-experimental"),
            v_flex()
                .px_3()
                .pb_2()
                .child(muted_text(i18n::t("settings-desc-memory"), muted))
                .into_any_element(),
            row_with_control(
                i18n::t("settings-row-memory-enabled"),
                Some(muted_text(i18n::t("settings-desc-memory-enabled"), muted)),
                mock_switch(
                    "memory-enabled",
                    view.memory_enabled,
                    entity.clone(),
                    Arc::new(|this, v| this.memory_enabled = v),
                ),
            ),
            hairline(theme.border.opacity(0.6)),
            row_with_control(
                i18n::t("settings-row-memory-skip-tool"),
                Some(muted_text(i18n::t("settings-desc-memory-skip-tool"), muted)),
                mock_switch(
                    "memory-skip-tool",
                    view.memory_skip_tool,
                    entity.clone(),
                    Arc::new(|this, v| this.memory_skip_tool = v),
                ),
            ),
            hairline(theme.border.opacity(0.6)),
            row_with_control(
                i18n::t("settings-row-memory-reset"),
                Some(muted_text(i18n::t("settings-desc-memory-reset"), muted)),
                Button::new("reset-memory")
                    .label(i18n::t("settings-btn-reset"))
                    .danger()
                    .on_click(|_ev, _window, _cx| {
                        tracing::info!("reset memory clicked (no-op in this build)");
                    })
                    .into_any_element(),
            ),
        ];
        section_card(&theme, children)
    };

    panel_scroll(
        v_flex()
            .w_full()
            .gap_4()
            .child(personality_section)
            .child(memory_section),
    )
}

// --- MCP panel ------------------------------------------------------------

pub fn render_mcp(view: &mut SettingsView, cx: &mut Context<SettingsView>) -> AnyElement {
    let theme = cx.theme().clone();
    let entity = cx.entity();
    let muted = theme.muted_foreground;

    let servers: Vec<String> = mcp::config::load_global()
        .mcp_servers
        .keys()
        .cloned()
        .collect();

    // Grow the on set with any newly-appearing servers so they default to on.
    let new_servers: Vec<String> = servers
        .iter()
        .filter(|n| !view.mcp_enabled.contains(*n))
        .cloned()
        .collect();
    for n in &new_servers {
        view.mcp_enabled.insert(n.clone());
    }

    let top_header = v_flex()
        .gap_1()
        .child(
            h_flex()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .text_base()
                        .font_weight(gpui::FontWeight::BLACK)
                        .child(i18n::t("settings-panel-mcp")),
                )
                .child(
                    Button::new("add-mcp")
                        .label(i18n::t("settings-btn-add-server"))
                        .outline()
                        .icon(Icon::new(IconName::Plus))
                        .on_click(|_ev, _window, _cx| {
                            tracing::info!("add MCP server clicked (no-op in this build)");
                        })
                        .into_any_element(),
                )
                .into_any_element(),
        )
        .child(muted_text(i18n::t("settings-desc-mcp"), muted))
        .into_any_element();

    let mut server_rows: Vec<AnyElement> = vec![section_header("settings-section-mcp-servers")];
    if servers.is_empty() {
        server_rows.push(
            v_flex()
                .px_3()
                .py_3()
                .child(muted_text(i18n::t("settings-empty-mcp"), muted))
                .into_any_element(),
        );
    } else {
        for (ix, name) in servers.iter().enumerate() {
            let entity = entity.clone();
            let name = name.clone();
            let checked = view.mcp_enabled.contains(&name);
            server_rows.push(row_with_control(
                i18n::t("settings-row-mcp-plugin-name"),
                None,
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().text_sm().child(name.clone()))
                    .child(Icon::new(IconName::Settings).small().text_color(muted))
                    .child(mock_switch(
                        format!("mcp-{}", name),
                        checked,
                        entity,
                        Arc::new(move |this, v| {
                            if v {
                                this.mcp_enabled.insert(name.clone());
                            } else {
                                this.mcp_enabled.remove(&name);
                            }
                        }),
                    ))
                    .into_any_element(),
            ));
            if ix + 1 < servers.len() {
                server_rows.push(hairline(theme.border.opacity(0.6)));
            }
        }
    }
    let servers_section = section_card(&theme, server_rows);

    let plugins_section = {
        let entity = entity.clone();
        let children = vec![
            section_header("settings-section-mcp-plugins"),
            row_with_control(
                i18n::t("settings-row-mcp-plugin-name"),
                None,
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .text_sm()
                            .child(i18n::t("settings-row-mcp-plugin-name")),
                    )
                    .child(mock_switch(
                        "mcp-plugin-manox-apps",
                        false,
                        entity,
                        Arc::new(|_this, _v| {}),
                    ))
                    .into_any_element(),
            ),
        ];
        section_card(&theme, children)
    };

    panel_scroll(
        v_flex()
            .w_full()
            .gap_4()
            .child(top_header)
            .child(servers_section)
            .child(plugins_section),
    )
}

// --- Environment panel ----------------------------------------------------

pub fn render_environment(_view: &mut SettingsView, cx: &mut Context<SettingsView>) -> AnyElement {
    // Mock panel: no SettingsView state is read or mutated. The constant
    // project list is rendered as a static two-column grid.
    let theme = cx.theme().clone();
    let muted = theme.muted_foreground;

    let top_header = v_flex()
        .gap_1()
        .child(
            h_flex()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .text_base()
                        .font_weight(gpui::FontWeight::BLACK)
                        .child(i18n::t("settings-panel-environment")),
                )
                .child(
                    Button::new("add-project")
                        .label(i18n::t("settings-btn-add-project"))
                        .outline()
                        .icon(Icon::new(IconName::Plus))
                        .on_click(|_ev, _window, _cx| {
                            tracing::info!("add project clicked (no-op in this build)");
                        })
                        .into_any_element(),
                )
                .into_any_element(),
        )
        .child(muted_text(i18n::t("settings-desc-environment"), muted))
        .into_any_element();

    let mut project_rows: Vec<AnyElement> = vec![section_header("settings-section-projects")];
    for (ix, project) in MOCK_PROJECTS.iter().enumerate() {
        project_rows.push(project_card(project, muted));
        if ix + 1 < MOCK_PROJECTS.len() {
            project_rows.push(hairline(theme.border.opacity(0.6)));
        }
    }
    let projects_section = section_card(&theme, project_rows);

    panel_scroll(
        v_flex()
            .w_full()
            .gap_4()
            .child(top_header)
            .child(projects_section),
    )
}

fn project_card(project: &MockProject, muted: Hsla) -> AnyElement {
    h_flex()
        .w_full()
        .items_center()
        .gap_3()
        .px_3()
        .py_3()
        .child(Icon::new(IconName::File).small().text_color(muted))
        .child(
            v_flex()
                .flex_1()
                .gap_0p5()
                .child(
                    div()
                        .text_sm()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .child(project.name),
                )
                .when_some(project.tag, |this, tag| {
                    let tag_text: SharedString = match tag {
                        "saas" => i18n::t("settings-tag-saas"),
                        "dspo" => i18n::t("settings-tag-dspo"),
                        _ => tag.into(),
                    };
                    this.child(div().text_xs().text_color(muted).child(tag_text))
                })
                .into_any_element(),
        )
        .child(
            Button::new(format!("proj-add-{}", project.name))
                .icon(Icon::new(IconName::Plus))
                .outline()
                .on_click(|_ev, _window, _cx| {
                    tracing::info!("add project entry clicked (no-op in this build)");
                })
                .into_any_element(),
        )
        .into_any_element()
}

fn section_header_with_tag(label: &'static str, tag: &'static str) -> AnyElement {
    h_flex()
        .items_center()
        .gap_2()
        .px_3()
        .pt_3()
        .pb_1()
        .child(
            div()
                .text_xs()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .child(i18n::t(label)),
        )
        .child(
            div()
                .text_xs()
                .px_1p5()
                .py_0p5()
                .rounded(px(4.))
                .bg(gpui::transparent_black())
                .text_color(gpui::transparent_black())
                .child(i18n::t(tag)),
        )
        .into_any_element()
}
