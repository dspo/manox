//! Plugin + marketplace management view (ported from the retired manox
//! harness, scoped to the plugin lifecycle).
//!
//! Two tabs: **Marketplace** (add/refresh/remove marketplaces, browse their
//! plugins, install) and **Plugin** (installed set with update / enable /
//! disable / uninstall). The retired view additionally managed user skills
//! and mcp.toml servers; those authoring surfaces need shared-layer write
//! APIs that do not exist yet (skill drafts, mcp config editing) and are
//! follow-ups — MCP servers are listed/toggled in the dedicated Settings →
//! MCP servers panel.
//!
//! All mutations run on the background executor through [`Self::run_task`]
//! (a busy spinner + success/error notice banner); registry changes take
//! effect on restart because the runtime registries load at startup — the
//! notice texts say so.

use crate::i18n;
use manox_agent::plugin::PluginManager;
use gpui::{AnyElement, Context, Entity, Hsla, Render, SharedString, Window, div, prelude::*, px};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, Theme,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputState},
    scroll::ScrollableElement as _,
    tab::TabBar,
    tag::{Tag, TagVariant},
    v_flex,
};

use crate::views::braille_spinner::BrailleSpinner;

#[derive(Clone, Copy, PartialEq, Eq)]
enum PluginManagerTab {
    Marketplace,
    Plugin,
}

pub struct PluginManagerView {
    tab: PluginManagerTab,
    search: Entity<InputState>,
    marketplace_url: Entity<InputState>,
    selected_marketplace: Option<String>,
    notice: Option<Notice>,
    busy: bool,
}

#[derive(Clone)]
struct Notice {
    text: SharedString,
    is_error: bool,
}

impl PluginManagerView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self {
            tab: PluginManagerTab::Marketplace,
            search: cx.new(|cx| {
                InputState::new(window, cx).placeholder(i18n::t("plugins-search-placeholder"))
            }),
            marketplace_url: cx.new(|cx| {
                InputState::new(window, cx).placeholder(i18n::t("plugins-marketplace-url"))
            }),
            selected_marketplace: None,
            notice: None,
            busy: false,
        }
    }

    fn run_task(
        &mut self,
        success: SharedString,
        op: impl FnOnce() -> anyhow::Result<()> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        self.busy = true;
        self.notice = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async move { op() }).await;
            this.update(cx, |this, cx| {
                this.busy = false;
                this.notice = Some(match result {
                    Ok(()) => Notice {
                        text: success,
                        is_error: false,
                    },
                    Err(e) => Notice {
                        text: e.to_string().into(),
                        is_error: true,
                    },
                });
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn add_marketplace(&mut self, cx: &mut Context<Self>) {
        let url = self.marketplace_url.read(cx).value().trim().to_string();
        if url.is_empty() {
            self.notice = Some(Notice {
                text: i18n::t("plugins-error-marketplace-url"),
                is_error: true,
            });
            cx.notify();
            return;
        }
        self.selected_marketplace = Some(manox_agent::paths::marketplace_slug(&url));
        self.run_task(
            i18n::t("plugins-notice-marketplace-added"),
            move || {
                PluginManager::add_marketplace(&url)?;
                Ok(())
            },
            cx,
        );
    }

    fn refresh_marketplace(&mut self, slug: String, cx: &mut Context<Self>) {
        self.run_task(
            i18n::t("plugins-notice-marketplace-updated"),
            move || {
                PluginManager::refresh_marketplace(&slug)?;
                Ok(())
            },
            cx,
        );
    }

    fn remove_marketplace(&mut self, slug: String, cx: &mut Context<Self>) {
        if self.selected_marketplace.as_deref() == Some(slug.as_str()) {
            self.selected_marketplace = None;
        }
        self.run_task(
            i18n::t("plugins-notice-marketplace-removed"),
            move || PluginManager::remove_marketplace_by_slug(&slug),
            cx,
        );
    }

    fn install_plugin(&mut self, marketplace: String, plugin: String, cx: &mut Context<Self>) {
        self.run_task(
            i18n::t("plugins-notice-plugin-installed"),
            move || PluginManager::install(&marketplace, &plugin),
            cx,
        );
    }

    fn uninstall_plugin(&mut self, plugin: String, cx: &mut Context<Self>) {
        self.run_task(
            i18n::t("plugins-notice-plugin-removed"),
            move || PluginManager::uninstall(&plugin),
            cx,
        );
    }

    fn set_plugin_enabled(&mut self, plugin: String, enabled: bool, cx: &mut Context<Self>) {
        self.run_task(
            if enabled {
                i18n::t("plugins-notice-plugin-enabled")
            } else {
                i18n::t("plugins-notice-plugin-disabled")
            },
            move || {
                if enabled {
                    PluginManager::enable(&plugin)
                } else {
                    PluginManager::disable(&plugin)
                }
            },
            cx,
        );
    }

    fn search_text(&self, cx: &mut Context<Self>) -> String {
        self.search.read(cx).value().trim().to_lowercase()
    }
}

impl Render for PluginManagerView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let tab_ix = match self.tab {
            PluginManagerTab::Marketplace => 0,
            PluginManagerTab::Plugin => 1,
        };
        let notice = self.notice.clone();
        let busy = self.busy;
        let search = self.search.clone();

        // The settings shell owns the window TitleBar (drag region + traffic
        // lights + section title); this view fills the content area below it.
        // The search field sits at the trailing edge of the tab row so a long
        // marketplace list can be filtered without a separate header band.
        let search_box = h_flex()
            .w(px(280.))
            .min_w_0()
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
            );

        v_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(
                h_flex()
                    .px_5()
                    .pt_3()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        TabBar::new("plugin-manager-tabs")
                            .underline()
                            .selected_index(tab_ix)
                            .on_click(cx.listener(|this, ix: &usize, _window, cx| {
                                this.tab = match *ix {
                                    0 => PluginManagerTab::Marketplace,
                                    _ => PluginManagerTab::Plugin,
                                };
                                cx.notify();
                            }))
                            .child(i18n::t("plugins-tab-marketplace"))
                            .child(i18n::t("plugins-tab-plugin")),
                    )
                    .child(search_box),
            )
            .children(notice.map(|notice| notice_banner(notice, &theme)))
            .when(busy, |el| {
                el.child(
                    h_flex()
                        .px_5()
                        .pt_2()
                        .gap_2()
                        .items_center()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(BrailleSpinner::new().small().color(theme.muted_foreground))
                        .child(i18n::t("plugins-busy")),
                )
            })
            .child(match self.tab {
                PluginManagerTab::Marketplace => self.render_marketplace(cx),
                PluginManagerTab::Plugin => self.render_plugins(cx),
            })
    }
}

impl PluginManagerView {
    fn render_marketplace(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let query = self.search_text(cx);
        let marketplaces: Vec<_> = PluginManager::list_marketplace_records()
            .into_iter()
            .filter(|m| {
                matches_query(
                    &query,
                    [
                        m.slug.as_str(),
                        m.name.as_str(),
                        m.description.as_deref().unwrap_or(""),
                    ],
                )
            })
            .collect();
        let selected = self
            .selected_marketplace
            .clone()
            .or_else(|| marketplaces.first().map(|m| m.slug.clone()));

        v_flex()
            .flex_1()
            .min_h_0()
            .overflow_y_scrollbar()
            .px_5()
            .py_4()
            .gap_4()
            .child(
                h_flex()
                    .gap_2()
                    .child(Input::new(&self.marketplace_url).bordered(true).flex_1())
                    .child(
                        Button::new("add-marketplace")
                            .primary()
                            .label(i18n::t("plugins-add-marketplace"))
                            .icon(Icon::new(IconName::Plus))
                            .disabled(self.busy)
                            .flex_shrink_0()
                            .on_click(cx.listener(|this, _, _, cx| this.add_marketplace(cx))),
                    ),
            )
            .child(
                h_flex()
                    .gap_4()
                    .items_start()
                    .child(
                        v_flex()
                            .w(px(360.))
                            .gap_2()
                            .children(if marketplaces.is_empty() {
                                vec![empty_state(i18n::t("plugins-empty-marketplaces"), &theme)]
                            } else {
                                marketplaces
                                    .iter()
                                    .map(|m| {
                                        marketplace_card(
                                            m,
                                            selected.as_deref() == Some(m.slug.as_str()),
                                            self.busy,
                                            cx,
                                        )
                                    })
                                    .collect()
                            }),
                    )
                    .child(self.render_marketplace_plugins(selected, cx)),
            )
            .into_any_element()
    }

    fn render_marketplace_plugins(
        &mut self,
        selected: Option<String>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let Some(slug) = selected else {
            return empty_state(i18n::t("plugins-empty-marketplace-selection"), &theme);
        };
        let rows = match PluginManager::list_marketplace_plugins(&slug) {
            Ok(plugins) if plugins.is_empty() => {
                vec![empty_state(
                    i18n::t("plugins-empty-marketplace-plugins"),
                    &theme,
                )]
            }
            Ok(plugins) => plugins
                .into_iter()
                .map(|plugin| marketplace_plugin_card(plugin, self.busy, cx))
                .collect(),
            Err(e) => vec![empty_state(e.to_string().into(), &theme)],
        };
        v_flex()
            .flex_1()
            .gap_2()
            .child(section_title(i18n::t_str(
                "plugins-marketplace-detail",
                &[("name", &slug)],
            )))
            .children(rows)
            .into_any_element()
    }

    fn render_plugins(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let query = self.search_text(cx);
        let plugins: Vec<_> = PluginManager::installed_details()
            .into_iter()
            .filter(|p| {
                matches_query(
                    &query,
                    [
                        p.name.as_str(),
                        p.marketplace.as_str(),
                        p.description.as_deref().unwrap_or(""),
                    ],
                )
            })
            .collect();
        v_flex()
            .flex_1()
            .min_h_0()
            .overflow_y_scrollbar()
            .px_5()
            .py_4()
            .gap_2()
            .children(if plugins.is_empty() {
                vec![empty_state(i18n::t("plugins-empty-installed"), &theme)]
            } else {
                plugins
                    .into_iter()
                    .map(|plugin| installed_plugin_card(plugin, self.busy, cx))
                    .collect()
            })
            .into_any_element()
    }
}

// ─── cards ──────────────────────────────────────────────────────────────────

fn marketplace_card(
    record: &manox_agent::plugin::MarketplaceRecord,
    selected: bool,
    busy: bool,
    cx: &mut Context<PluginManagerView>,
) -> AnyElement {
    let theme = cx.theme().clone();
    let slug_select = record.slug.clone();
    let slug_refresh = record.slug.clone();
    let slug_remove = record.slug.clone();
    item_card(&theme, selected)
        .child(
            h_flex()
                .justify_between()
                .gap_2()
                .child(item_text(
                    record.name.clone(),
                    format!(
                        "{} · {}",
                        i18n::t_str(
                            "plugins-marketplace-count",
                            &[("count", &record.plugin_count.to_string())]
                        ),
                        record
                            .git_url
                            .as_deref()
                            .unwrap_or_else(|| record.root.to_str().unwrap_or(""))
                    ),
                    record.description.clone(),
                    &theme,
                ))
                .child(
                    h_flex()
                        .flex_shrink_0()
                        .gap_1()
                        .child(
                            Button::new(format!("select-marketplace-{}", record.slug))
                                .small()
                                .outline()
                                .label(i18n::t("plugins-select"))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.selected_marketplace = Some(slug_select.clone());
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new(format!("refresh-marketplace-{}", record.slug))
                                .small()
                                .outline()
                                .label(i18n::t("plugins-update"))
                                .disabled(busy)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.refresh_marketplace(slug_refresh.clone(), cx);
                                })),
                        )
                        .child(
                            Button::new(format!("remove-marketplace-{}", record.slug))
                                .small()
                                .danger()
                                .label(i18n::t("plugins-delete"))
                                .disabled(busy)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.remove_marketplace(slug_remove.clone(), cx);
                                })),
                        ),
                ),
        )
        .into_any_element()
}

fn marketplace_plugin_card(
    plugin: manox_agent::plugin::MarketplacePluginRecord,
    busy: bool,
    cx: &mut Context<PluginManagerView>,
) -> AnyElement {
    let theme = cx.theme().clone();
    let marketplace = plugin.marketplace_slug.clone();
    let name = plugin.name.clone();
    let name_install = plugin.name.clone();
    let marketplace_install = plugin.marketplace_slug.clone();
    let name_action = plugin.name.clone();
    let marketplace_action = plugin.marketplace_slug.clone();
    let name_toggle = plugin.name.clone();
    let marketplace_toggle = plugin.marketplace_slug.clone();
    let name_uninstall = plugin.name.clone();
    let installed = plugin.installed;
    let enabled = plugin.enabled;
    let (tag_label, tag_active) = if !installed {
        (i18n::t("plugins-not-installed"), false)
    } else if enabled {
        (i18n::t("plugins-installed"), true)
    } else {
        (i18n::t("plugins-disabled"), false)
    };
    item_card(&theme, false)
        .child(
            h_flex()
                .justify_between()
                .gap_3()
                .child(item_text(name, plugin.source, plugin.description, &theme))
                .child(
                    h_flex()
                        .flex_shrink_0()
                        .gap_1()
                        .child(status_tag(tag_label, tag_active))
                        .when(!installed, |el| {
                            el.child(
                                Button::new(format!("install-{}-{}", marketplace, name_install))
                                    .small()
                                    .primary()
                                    .label(i18n::t("plugins-install"))
                                    .disabled(busy)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.install_plugin(
                                            marketplace_install.clone(),
                                            name_install.clone(),
                                            cx,
                                        );
                                    })),
                            )
                        })
                        .when(installed, |el| {
                            el.child(
                                Button::new(format!("update-{}-{}", marketplace, name_action))
                                    .small()
                                    .outline()
                                    .label(i18n::t("plugins-update"))
                                    .disabled(busy)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.install_plugin(
                                            marketplace_action.clone(),
                                            name_action.clone(),
                                            cx,
                                        );
                                    })),
                            )
                        })
                        .when(installed, |el| {
                            el.child(
                                Button::new(format!(
                                    "toggle-{}-{}",
                                    marketplace_toggle, name_toggle
                                ))
                                .small()
                                .outline()
                                .label(if enabled {
                                    i18n::t("plugins-disable")
                                } else {
                                    i18n::t("plugins-enable")
                                })
                                .disabled(busy)
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.set_plugin_enabled(name_toggle.clone(), !enabled, cx);
                                    },
                                )),
                            )
                        })
                        .when(installed, |el| {
                            el.child(
                                Button::new(format!(
                                    "uninstall-{}-{}",
                                    marketplace, name_uninstall
                                ))
                                .small()
                                .danger()
                                .label(i18n::t("plugins-uninstall"))
                                .disabled(busy)
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.uninstall_plugin(name_uninstall.clone(), cx);
                                    },
                                )),
                            )
                        }),
                ),
        )
        .into_any_element()
}

fn installed_plugin_card(
    plugin: manox_agent::plugin::InstalledPluginRecord,
    busy: bool,
    cx: &mut Context<PluginManagerView>,
) -> AnyElement {
    let theme = cx.theme().clone();
    let name_update = plugin.name.clone();
    let market_update = plugin.marketplace.clone();
    let name_toggle = plugin.name.clone();
    let name_delete = plugin.name.clone();
    let can_update = !plugin.marketplace.is_empty();
    let enabled = plugin.enabled;
    let subtitle = format!(
        "{}{}",
        plugin.marketplace,
        plugin
            .version
            .as_deref()
            .map(|v| format!(" · v{v}"))
            .unwrap_or_default()
    );
    item_card(&theme, false)
        .child(
            h_flex()
                .justify_between()
                .gap_3()
                .child(item_text(
                    plugin.name,
                    subtitle,
                    plugin
                        .description
                        .or_else(|| plugin.root.to_str().map(|path| path.to_string())),
                    &theme,
                ))
                .child(
                    h_flex()
                        .flex_shrink_0()
                        .gap_1()
                        .child(status_tag(
                            if enabled {
                                i18n::t("plugins-enabled")
                            } else {
                                i18n::t("plugins-disabled")
                            },
                            enabled,
                        ))
                        .children(can_update.then(|| {
                            Button::new(format!("update-installed-{}", name_update))
                                .small()
                                .outline()
                                .label(i18n::t("plugins-update"))
                                .disabled(busy)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.install_plugin(
                                        market_update.clone(),
                                        name_update.clone(),
                                        cx,
                                    );
                                }))
                                .into_any_element()
                        }))
                        .child(
                            Button::new(format!("toggle-installed-{}", name_toggle))
                                .small()
                                .outline()
                                .label(if enabled {
                                    i18n::t("plugins-disable")
                                } else {
                                    i18n::t("plugins-enable")
                                })
                                .disabled(busy)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.set_plugin_enabled(name_toggle.clone(), !enabled, cx);
                                })),
                        )
                        .child(
                            Button::new(format!("uninstall-{}", name_delete))
                                .small()
                                .danger()
                                .label(i18n::t("plugins-uninstall"))
                                .disabled(busy)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.uninstall_plugin(name_delete.clone(), cx);
                                })),
                        ),
                ),
        )
        .into_any_element()
}

// ─── shared bits ────────────────────────────────────────────────────────────

fn section_title(label: SharedString) -> AnyElement {
    div()
        .text_sm()
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .child(label)
        .into_any_element()
}

fn empty_state(label: SharedString, theme: &Theme) -> AnyElement {
    div()
        .w_full()
        .p_4()
        .rounded(px(8.))
        .border_1()
        .border_color(theme.border)
        .text_sm()
        .text_color(theme.muted_foreground)
        .child(label)
        .into_any_element()
}

fn status_tag(label: SharedString, active: bool) -> AnyElement {
    Tag::new()
        .with_variant(if active {
            TagVariant::Primary
        } else {
            TagVariant::Secondary
        })
        .small()
        .child(label)
        .into_any_element()
}

fn notice_banner(notice: Notice, theme: &Theme) -> AnyElement {
    let color: Hsla = if notice.is_error {
        theme.danger
    } else {
        theme.success
    };
    h_flex()
        .mx_5()
        .mt_3()
        .px_3()
        .py_2()
        .rounded(px(8.))
        .bg(color.opacity(0.08))
        .text_sm()
        .text_color(color)
        .child(notice.text)
        .into_any_element()
}

fn matches_query<'a>(query: &str, fields: impl IntoIterator<Item = &'a str>) -> bool {
    query.is_empty()
        || fields
            .into_iter()
            .any(|field| field.to_lowercase().contains(query))
}

fn item_card(theme: &Theme, selected: bool) -> gpui::Div {
    let bg = if selected {
        theme.accent.opacity(0.12)
    } else {
        theme.secondary.opacity(0.42)
    };
    v_flex()
        .w_full()
        .p_3()
        .gap_2()
        .rounded(px(8.))
        .border_1()
        .border_color(if selected { theme.accent } else { theme.border })
        .bg(bg)
}

fn item_text(
    title: impl Into<SharedString>,
    subtitle: impl Into<SharedString>,
    description: Option<String>,
    theme: &Theme,
) -> AnyElement {
    v_flex()
        .flex_1()
        .min_w_0()
        .gap_1()
        .child(
            div()
                .text_sm()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .min_w_0()
                .truncate()
                .child(title.into()),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .min_w_0()
                // Subtitle often carries a Git URL or filesystem path —
                // truncate so a long incompressible run can't blow the row
                // out under a narrow window. Description below wraps
                // instead, since it is prose the user may want to read in
                // full.
                .truncate()
                .child(subtitle.into()),
        )
        .children(description.filter(|s| !s.is_empty()).map(|description| {
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .min_w_0()
                .child(description)
        }))
        .into_any_element()
}
