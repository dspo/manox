//! Settings overlay — a single-window alternative to opening a separate
//! preferences window. Mounts inline over the Workspace via
//! `Workspace::view_mode`; clicks on sidebar items update a local
//! `selected` highlight and the right pane dispatches to one of five
//! Codex-style panels (General / Config / Personalization / MCP /
//! Environment). Items with no matching panel fall back to a "Coming soon…"
//! placeholder, matching the pre-panels behavior.

use std::collections::HashSet;

use gpui::{
    Animation, AnimationExt as _, AnyElement, Context, Entity, EventEmitter, Render, SharedString,
    Window, ease_out_quint, prelude::*, px,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, TITLE_BAR_HEIGHT, TitleBar, h_flex,
    input::{Input, InputState},
    v_flex,
};

use agent::{i18n, mcp, settings as user_settings};

mod panels;

const SIDEBAR_W: f32 = 260.;
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

/// Work mode preference in the General panel. Two-card selector mirrors the
/// "For Programming / For Daily Work" pair in the Codex screenshot. Persisted to
/// `Settings` only conceptually — the field is read by the panel on render
/// and any in-memory edit stays local until a follow-up wires it through.
#[derive(Clone, Copy, PartialEq, Default)]
pub enum WorkMode {
    #[default]
    Programming,
    Workday,
}

/// A mocked project entry in the Environment panel. The `tag` renders as a
/// pill next to the project name (e.g. "saas" or "dspo"). Real project data
/// is not yet modeled, so all entries are static placeholders.
pub struct MockProject {
    pub name: &'static str,
    pub tag: Option<&'static str>,
}

const MOCK_PROJECTS: &[MockProject] = &[
    MockProject {
        name: "cvat",
        tag: None,
    },
    MockProject {
        name: "floraldet-training",
        tag: Some("saas"),
    },
    MockProject {
        name: "cx",
        tag: Some("dspo"),
    },
    MockProject {
        name: "huaji-skm",
        tag: Some("saas"),
    },
];

pub struct SettingsView {
    search: Entity<InputState>,

    /// Sidebar item currently highlighted. Stable fluent message id (e.g.
    /// `"settings-item-general"`) so it survives locale switches.
    selected: Option<SharedString>,

    /// Bumped on every click so the click-flash animation re-fires.
    click_gen: u64,

    /// Multi-line text input backing the "Custom instructions" textarea.
    custom_instructions_input: Entity<InputState>,

    // --- General panel state ---
    work_mode: WorkMode,
    permission_default: bool,
    permission_auto_review: bool,
    permission_full_access: bool,
    file_target: SharedString,
    language: SharedString,
    show_in_menu_bar: bool,
    bottom_panel: bool,
    terminal_location: SharedString,
    keep_awake: bool,
    code_review_mode: SharedString,
    send_shortcut: SharedString,
    follow_up_behavior: SharedString,
    pop_up_shortcut_status: SharedString,
    default_no_project_chat: bool,
    microphone: SharedString,
    press_dictate_status: SharedString,
    toggle_dictate_status: SharedString,
    keep_dictation_bar: bool,
    turn_completion_notify: SharedString,
    permission_notify: bool,
    question_notify: bool,

    // --- Config panel state ---
    config_user_target: SharedString,
    config_approval_policy: SharedString,
    config_sandbox: SharedString,
    config_codex_deps: bool,

    // --- Personalization panel state ---
    personality: SharedString,
    memory_enabled: bool,
    memory_skip_tool: bool,
    /// `true` for ~2s after a successful save, then reverts. Drives the save
    /// button's transient "Saved" label.
    just_saved: bool,

    // --- MCP panel state ---
    /// Servers currently "on" in the UI. The toggle is visual only — the
    /// actual set of connected servers is determined by `mcp.toml` on the
    /// next launch — so this set is seeded from the disk config and grown
    /// as new servers appear.
    mcp_enabled: HashSet<String>,
    // --- Environment panel state ---
    // Mock project list is static; no per-view state needed.
}

#[derive(Clone)]
pub enum SettingsEvent {
    Exit,
}

impl EventEmitter<SettingsEvent> for SettingsView {}

impl SettingsView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| {
            InputState::new(window, cx).placeholder(i18n::t("settings-search-placeholder"))
        });
        let custom_instructions_input = cx.new(|cx| {
            let mut state = InputState::new(window, cx)
                .placeholder(i18n::t("settings-input-custom-instructions"));
            // Pre-fill the textarea with the persisted value, if any, so the
            // user can edit their previous instructions rather than start over.
            if let Some(prior) = user_settings::load().custom_instructions {
                state = state.default_value(prior);
            }
            state
        });

        // Seed MCP enabled set from the on-disk config so every configured
        // server starts in the "on" position.
        let mut mcp_enabled = HashSet::new();
        for name in mcp::config::load_global().mcp_servers.keys() {
            mcp_enabled.insert(name.clone());
        }

        Self {
            search,
            selected: None,
            click_gen: 0,
            custom_instructions_input,
            work_mode: WorkMode::default(),
            permission_default: true,
            permission_auto_review: false,
            permission_full_access: false,
            file_target: i18n::t("settings-value-vscode"),
            language: {
                // Display the saved token in a human-readable form; the
                // underlying `Settings::language` holds the locale tag.
                let saved = user_settings::load().language.unwrap_or_default();
                match saved.as_str() {
                    "" => i18n::t("settings-value-auto-detect"),
                    "en" => i18n::t("settings-value-en"),
                    "zh-CN" => i18n::t("settings-value-zh-CN"),
                    _ => saved.into(),
                }
            },
            show_in_menu_bar: true,
            bottom_panel: true,
            terminal_location: i18n::t("settings-value-bottom"),
            keep_awake: true,
            code_review_mode: i18n::t("settings-value-detached"),
            send_shortcut: i18n::t("settings-value-enter-shift"),
            follow_up_behavior: i18n::t("settings-value-queue"),
            pop_up_shortcut_status: i18n::t("settings-value-disabled"),
            default_no_project_chat: false,
            microphone: i18n::t("settings-value-system-default"),
            press_dictate_status: i18n::t("settings-value-off"),
            toggle_dictate_status: i18n::t("settings-value-off"),
            keep_dictation_bar: false,
            turn_completion_notify: i18n::t("settings-value-focus-only"),
            permission_notify: true,
            question_notify: true,
            config_user_target: i18n::t("settings-value-on"),
            config_approval_policy: i18n::t("settings-value-on-request"),
            config_sandbox: i18n::t("settings-value-read-only"),
            config_codex_deps: true,
            personality: i18n::t("settings-value-friendly"),
            memory_enabled: false,
            memory_skip_tool: false,
            just_saved: false,
            mcp_enabled,
        }
    }

    fn persist_custom_instructions(&mut self, cx: &mut Context<Self>) {
        // Read the current value out of the InputState entity.
        let value = self.custom_instructions_input.read(cx).value();
        let mut settings = user_settings::load();
        let trimmed = value.trim().to_string();
        settings.custom_instructions = if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        };
        match user_settings::save(&settings) {
            Ok(()) => {
                self.just_saved = true;
                cx.notify();
                // Revert the button label back to "Save" after a short delay
                // so the user sees the confirmation without it sticking.
                let weak = cx.weak_entity();
                cx.spawn(async move |_this, cx| {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(1600))
                        .await;
                    if let Some(this) = weak.upgrade() {
                        this.update(cx, |view, cx| {
                            view.just_saved = false;
                            cx.notify();
                        });
                    }
                })
                .detach();
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to save custom_instructions");
            }
        }
    }

    fn persist_language(&mut self, value: SharedString, cx: &mut Context<Self>) {
        // The dropdown option is a (display_label, persist_token) pair; only
        // the token is written to settings.toml. An empty string means the user
        // chose the "auto-detect" placeholder, which should leave the existing
        // setting alone rather than clear it.
        let token = value.to_string();
        if token.is_empty() {
            return;
        }
        let mut settings = user_settings::load();
        settings.language = Some(token);
        if let Err(e) = user_settings::save(&settings) {
            tracing::warn!(error = %e, "failed to save language");
        }
        cx.notify();
    }
}

impl Render for SettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let selected = self.selected.clone();
        let search = self.search.clone();

        let on_back = cx.listener(|_this, _ev, _window, cx| {
            cx.emit(SettingsEvent::Exit);
        });

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
                    .active(|s| s.bg(theme.accent.opacity(0.24)))
                    .cursor_pointer()
                    .on_click(on_click)
                    .child(Icon::new(icon).small().text_color(theme.muted_foreground))
                    .child(gpui::div().flex_1().min_w_0().child(label_str.clone()));
                if let Some(t) = trailing {
                    row = row.child(Icon::new(t).small().text_color(theme.muted_foreground));
                }
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

        v_flex()
            .w_full()
            .h_full()
            .bg(theme.background)
            // Title bar spans the full window width: it carries the macOS
            // traffic-light inset and gives the user a drag handle to move the
            // window while Settings is mounted.
            .child(TitleBar::new().h(TITLE_BAR_HEIGHT))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .w_full()
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
                                    .min_h_0()
                                    .overflow_y_scroll()
                                    .px_2()
                                    .pb_2()
                                    .gap_3()
                                    .children(groups),
                            ),
                    )
                    .child(self.render_right_pane(&theme, cx)),
            )
    }
}

impl SettingsView {
    /// Dispatch the right pane based on the currently-selected sidebar item.
    /// Items without a dedicated panel fall through to a "Coming soon…"
    /// placeholder so the existing user-facing behavior for un-shipped panels
    /// (Appearance, Pets, Keyboard, …) is preserved verbatim.
    fn render_right_pane(
        &mut self,
        theme: &gpui_component::theme::Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = self.selected.as_deref();
        match key {
            Some("settings-item-general") => panels::render_general(self, cx).into_any_element(),
            Some("settings-item-config") => panels::render_config(self, cx).into_any_element(),
            Some("settings-item-personalization") => {
                panels::render_personalization(self, cx).into_any_element()
            }
            Some("settings-item-mcp") => panels::render_mcp(self, cx).into_any_element(),
            Some("settings-item-environment") => {
                panels::render_environment(self, cx).into_any_element()
            }
            _ => {
                let coming_label: SharedString = match key {
                    Some(label) => {
                        let displayed = i18n::t(label);
                        i18n::t_str(
                            "settings-coming-soon-label",
                            &[("label", displayed.as_ref())],
                        )
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
                    .into_any_element()
            }
        }
    }
}
