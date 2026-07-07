//! Plugin, skill, marketplace, and MCP management view.

use agent::{
    i18n,
    mcp::config::{self as mcp_config, McpServerConfig, McpServerTransportConfig},
    plugin::PluginManager,
    skill::{self, SkillOrigin, UserSkillDraft},
};
use gpui::{
    AnyElement, Context, Entity, EventEmitter, Hsla, Render, SharedString, Window, div, prelude::*,
    px,
};
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

#[derive(Clone)]
pub enum PluginManagerEvent {
    Exit,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PluginManagerTab {
    Marketplace,
    Plugin,
    Skill,
    Mcp,
}

pub struct PluginManagerView {
    tab: PluginManagerTab,
    search: Entity<InputState>,
    marketplace_url: Entity<InputState>,
    skill_name: Entity<InputState>,
    skill_description: Entity<InputState>,
    skill_body: Entity<InputState>,
    mcp_name: Entity<InputState>,
    mcp_command: Entity<InputState>,
    mcp_args: Entity<InputState>,
    mcp_url: Entity<InputState>,
    selected_marketplace: Option<String>,
    editing_skill_name: Option<String>,
    editing_mcp_name: Option<String>,
    notice: Option<Notice>,
    busy: bool,
}

#[derive(Clone)]
struct Notice {
    text: SharedString,
    is_error: bool,
}

impl EventEmitter<PluginManagerEvent> for PluginManagerView {}

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
            skill_name: cx
                .new(|cx| InputState::new(window, cx).placeholder(i18n::t("plugins-skill-name"))),
            skill_description: cx
                .new(|cx| InputState::new(window, cx).placeholder(i18n::t("plugins-description"))),
            skill_body: cx.new(|cx| {
                InputState::new(window, cx)
                    .multi_line(true)
                    .auto_grow(6, 16)
                    .placeholder(i18n::t("plugins-skill-body"))
            }),
            mcp_name: cx
                .new(|cx| InputState::new(window, cx).placeholder(i18n::t("plugins-mcp-name"))),
            mcp_command: cx
                .new(|cx| InputState::new(window, cx).placeholder(i18n::t("plugins-mcp-command"))),
            mcp_args: cx
                .new(|cx| InputState::new(window, cx).placeholder(i18n::t("plugins-mcp-args"))),
            mcp_url: cx
                .new(|cx| InputState::new(window, cx).placeholder(i18n::t("plugins-mcp-url"))),
            selected_marketplace: None,
            editing_skill_name: None,
            editing_mcp_name: None,
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
        self.selected_marketplace = Some(agent::paths::marketplace_slug(&url));
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

    fn edit_skill(
        &mut self,
        record: skill::SkillRecord,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editing_skill_name = Some(record.name.clone());
        self.skill_name
            .update(cx, |s, cx| s.set_value(record.name, window, cx));
        self.skill_description
            .update(cx, |s, cx| s.set_value(record.description, window, cx));
        self.skill_body
            .update(cx, |s, cx| s.set_value(record.body, window, cx));
        cx.notify();
    }

    fn new_skill(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editing_skill_name = None;
        self.skill_name
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.skill_description
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.skill_body
            .update(cx, |s, cx| s.set_value("", window, cx));
        cx.notify();
    }

    fn save_skill(&mut self, cx: &mut Context<Self>) {
        let draft = UserSkillDraft {
            name: self.skill_name.read(cx).value().trim().to_string(),
            description: self.skill_description.read(cx).value().trim().to_string(),
            body: self.skill_body.read(cx).value().to_string(),
        };
        let previous = self.editing_skill_name.clone();
        self.editing_skill_name = Some(draft.name.clone());
        self.run_task(
            i18n::t("plugins-notice-skill-saved"),
            move || skill::save_user_skill(&draft, previous.as_deref()),
            cx,
        );
    }

    fn delete_skill(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing_skill_name.as_deref() == Some(name.as_str()) {
            self.new_skill(window, cx);
        }
        self.run_task(
            i18n::t("plugins-notice-skill-removed"),
            move || skill::remove_user_skill(&name),
            cx,
        );
    }

    fn edit_mcp(
        &mut self,
        name: String,
        cfg: McpServerConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editing_mcp_name = Some(name.clone());
        self.mcp_name
            .update(cx, |s, cx| s.set_value(name, window, cx));
        match cfg.transport {
            McpServerTransportConfig::Stdio { command, args, .. } => {
                self.mcp_command
                    .update(cx, |s, cx| s.set_value(command, window, cx));
                self.mcp_args
                    .update(cx, |s, cx| s.set_value(args.join(" "), window, cx));
                self.mcp_url.update(cx, |s, cx| s.set_value("", window, cx));
            }
            McpServerTransportConfig::StreamableHttp { url, .. } => {
                self.mcp_command
                    .update(cx, |s, cx| s.set_value("", window, cx));
                self.mcp_args
                    .update(cx, |s, cx| s.set_value("", window, cx));
                self.mcp_url
                    .update(cx, |s, cx| s.set_value(url, window, cx));
            }
        }
        cx.notify();
    }

    fn new_mcp(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editing_mcp_name = None;
        self.mcp_name
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.mcp_command
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.mcp_args
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.mcp_url.update(cx, |s, cx| s.set_value("", window, cx));
        cx.notify();
    }

    fn save_mcp(&mut self, cx: &mut Context<Self>) {
        let name = self.mcp_name.read(cx).value().trim().to_string();
        let command = self.mcp_command.read(cx).value().trim().to_string();
        let args = self.mcp_args.read(cx).value().to_string();
        let url = self.mcp_url.read(cx).value().trim().to_string();
        let previous = self.editing_mcp_name.clone();
        self.editing_mcp_name = Some(name.clone());
        self.run_task(
            i18n::t("plugins-notice-mcp-saved"),
            move || {
                if name.is_empty() {
                    anyhow::bail!("MCP server name cannot be empty");
                }
                let mut cfg = mcp_config::load_global();
                if let Some(previous) = previous
                    && previous != name
                {
                    cfg.mcp_servers.remove(&previous);
                }
                let server = if !url.is_empty() {
                    McpServerConfig {
                        transport: McpServerTransportConfig::StreamableHttp { url, headers: None },
                    }
                } else if !command.is_empty() {
                    McpServerConfig {
                        transport: McpServerTransportConfig::Stdio {
                            command,
                            args: split_args(&args),
                            env: None,
                            cwd: None,
                        },
                    }
                } else {
                    anyhow::bail!("MCP server needs a command or URL");
                };
                cfg.mcp_servers.insert(name, server);
                mcp_config::save_global(&cfg)
            },
            cx,
        );
    }

    fn delete_mcp(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing_mcp_name.as_deref() == Some(name.as_str()) {
            self.new_mcp(window, cx);
        }
        self.run_task(
            i18n::t("plugins-notice-mcp-removed"),
            move || {
                let mut cfg = mcp_config::load_global();
                cfg.mcp_servers.remove(&name);
                mcp_config::save_global(&cfg)
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
            PluginManagerTab::Skill => 2,
            PluginManagerTab::Mcp => 3,
        };
        let notice = self.notice.clone();
        let busy = self.busy;
        let search = self.search.clone();

        v_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(
                h_flex()
                    .h(px(56.))
                    .px_5()
                    .items_center()
                    .justify_between()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        h_flex()
                            .items_center()
                            .gap_3()
                            .child(
                                Button::new("plugin-manager-back")
                                    .ghost()
                                    .small()
                                    .icon(Icon::new(IconName::ArrowLeft))
                                    .label(i18n::t("settings-back"))
                                    .on_click(cx.listener(|_, _, _, cx| {
                                        cx.emit(PluginManagerEvent::Exit);
                                    })),
                            )
                            .child(
                                div()
                                    .text_lg()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .child(i18n::t("plugins-title")),
                            ),
                    )
                    .child(
                        h_flex()
                            .w(px(280.))
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
                h_flex().px_5().pt_3().child(
                    TabBar::new("plugin-manager-tabs")
                        .underline()
                        .selected_index(tab_ix)
                        .on_click(cx.listener(|this, ix: &usize, _window, cx| {
                            this.tab = match *ix {
                                0 => PluginManagerTab::Marketplace,
                                1 => PluginManagerTab::Plugin,
                                2 => PluginManagerTab::Skill,
                                _ => PluginManagerTab::Mcp,
                            };
                            cx.notify();
                        }))
                        .child(i18n::t("plugins-tab-marketplace"))
                        .child(i18n::t("plugins-tab-plugin"))
                        .child(i18n::t("plugins-tab-skill"))
                        .child(i18n::t("plugins-tab-mcp")),
                ),
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
                        .child(Icon::new(IconName::LoaderCircle).small())
                        .child(i18n::t("plugins-busy")),
                )
            })
            .child(match self.tab {
                PluginManagerTab::Marketplace => self.render_marketplace(cx),
                PluginManagerTab::Plugin => self.render_plugins(cx),
                PluginManagerTab::Skill => self.render_skills(_window, cx),
                PluginManagerTab::Mcp => self.render_mcp(_window, cx),
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

    fn render_skills(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let query = self.search_text(cx);
        let skills: Vec<_> = skill::list_skill_records()
            .into_iter()
            .filter(|s| {
                matches_query(
                    &query,
                    [s.key.as_str(), s.name.as_str(), s.description.as_str()],
                )
            })
            .collect();
        v_flex()
            .flex_1()
            .min_h_0()
            .overflow_y_scrollbar()
            .px_5()
            .py_4()
            .gap_4()
            .child(self.render_skill_form(window, cx))
            .child(v_flex().gap_2().children(if skills.is_empty() {
                vec![empty_state(i18n::t("plugins-empty-skills"), &theme)]
            } else {
                skills
                    .into_iter()
                    .map(|record| skill_card(record, self.busy, window, cx))
                    .collect()
            }))
            .into_any_element()
    }

    fn render_skill_form(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let title = if self.editing_skill_name.is_some() {
            i18n::t("plugins-skill-edit")
        } else {
            i18n::t("plugins-skill-new")
        };
        form_card(
            title,
            vec![
                Input::new(&self.skill_name)
                    .bordered(true)
                    .w_full()
                    .into_any_element(),
                Input::new(&self.skill_description)
                    .bordered(true)
                    .w_full()
                    .into_any_element(),
                Input::new(&self.skill_body)
                    .bordered(true)
                    .w_full()
                    .h(px(160.))
                    .into_any_element(),
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("new-skill")
                            .outline()
                            .label(i18n::t("plugins-new"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.new_skill(window, cx);
                            })),
                    )
                    .child(
                        Button::new("save-skill")
                            .primary()
                            .label(i18n::t("settings-btn-save"))
                            .disabled(self.busy)
                            .on_click(cx.listener(|this, _, _, cx| this.save_skill(cx))),
                    )
                    .into_any_element(),
            ],
            cx,
        )
    }

    fn render_mcp(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let query = self.search_text(cx);
        let user_cfg = mcp_config::load_global();
        let mut user_servers: Vec<(String, McpServerConfig)> = user_cfg
            .mcp_servers
            .into_iter()
            .filter(|(name, cfg)| matches_query(&query, [name.as_str(), mcp_summary(cfg).as_str()]))
            .collect();
        user_servers.sort_by(|a, b| a.0.cmp(&b.0));
        let plugin_servers: Vec<_> = mcp_config::list_plugin_declared_servers()
            .into_iter()
            .filter(|s| {
                matches_query(
                    &query,
                    [
                        s.name.as_str(),
                        s.plugin.as_str(),
                        mcp_summary(&s.config).as_str(),
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
            .gap_4()
            .child(self.render_mcp_form(window, cx))
            .child(section_title(i18n::t("plugins-mcp-user")))
            .children(if user_servers.is_empty() {
                vec![empty_state(i18n::t("plugins-empty-mcp"), &theme)]
            } else {
                user_servers
                    .into_iter()
                    .map(|(name, cfg)| mcp_card(name, cfg, false, self.busy, window, cx))
                    .collect()
            })
            .child(section_title(i18n::t("plugins-mcp-plugin")))
            .children(if plugin_servers.is_empty() {
                vec![empty_state(i18n::t("plugins-empty-plugin-mcp"), &theme)]
            } else {
                plugin_servers
                    .into_iter()
                    .map(|server| plugin_mcp_card(server, &theme))
                    .collect()
            })
            .into_any_element()
    }

    fn render_mcp_form(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let title = if self.editing_mcp_name.is_some() {
            i18n::t("plugins-mcp-edit")
        } else {
            i18n::t("plugins-mcp-new")
        };
        form_card(
            title,
            vec![
                Input::new(&self.mcp_name)
                    .bordered(true)
                    .w_full()
                    .into_any_element(),
                h_flex()
                    .gap_2()
                    .child(
                        Input::new(&self.mcp_command)
                            .bordered(true)
                            .flex_1()
                            .into_any_element(),
                    )
                    .child(
                        Input::new(&self.mcp_args)
                            .bordered(true)
                            .flex_1()
                            .into_any_element(),
                    )
                    .into_any_element(),
                Input::new(&self.mcp_url)
                    .bordered(true)
                    .w_full()
                    .into_any_element(),
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("new-mcp")
                            .outline()
                            .label(i18n::t("plugins-new"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.new_mcp(window, cx);
                            })),
                    )
                    .child(
                        Button::new("save-mcp")
                            .primary()
                            .label(i18n::t("settings-btn-save"))
                            .disabled(self.busy)
                            .on_click(cx.listener(|this, _, _, cx| this.save_mcp(cx))),
                    )
                    .into_any_element(),
            ],
            cx,
        )
    }
}

fn marketplace_card(
    record: &agent::plugin::MarketplaceRecord,
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
    plugin: agent::plugin::MarketplacePluginRecord,
    busy: bool,
    cx: &mut Context<PluginManagerView>,
) -> AnyElement {
    let theme = cx.theme().clone();
    let marketplace = plugin.marketplace_slug.clone();
    let name = plugin.name.clone();
    let name_action = plugin.name.clone();
    let marketplace_action = plugin.marketplace_slug.clone();
    let installed = plugin.installed;
    item_card(&theme, false)
        .child(
            h_flex()
                .justify_between()
                .gap_3()
                .child(item_text(name, plugin.source, plugin.description, &theme))
                .child(
                    h_flex()
                        .gap_1()
                        .child(status_tag(
                            if installed {
                                i18n::t("plugins-installed")
                            } else {
                                i18n::t("plugins-not-installed")
                            },
                            installed,
                        ))
                        .child(
                            Button::new(format!("install-{}-{}", marketplace, name_action))
                                .small()
                                .outline()
                                .label(if installed {
                                    i18n::t("plugins-update")
                                } else {
                                    i18n::t("plugins-install")
                                })
                                .disabled(busy)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.install_plugin(
                                        marketplace_action.clone(),
                                        name_action.clone(),
                                        cx,
                                    );
                                })),
                        ),
                ),
        )
        .into_any_element()
}

fn installed_plugin_card(
    plugin: agent::plugin::InstalledPluginRecord,
    busy: bool,
    cx: &mut Context<PluginManagerView>,
) -> AnyElement {
    let theme = cx.theme().clone();
    let name_update = plugin.name.clone();
    let market_update = plugin.marketplace.clone();
    let name_delete = plugin.name.clone();
    let can_update = !plugin.marketplace.is_empty();
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
                        .gap_1()
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

fn skill_card(
    record: skill::SkillRecord,
    busy: bool,
    _window: &mut Window,
    cx: &mut Context<PluginManagerView>,
) -> AnyElement {
    let theme = cx.theme().clone();
    let is_user = matches!(record.origin, SkillOrigin::User);
    let source = record.source.to_string_lossy().to_string();
    let origin = match &record.origin {
        SkillOrigin::User => i18n::t("plugins-origin-user").to_string(),
        SkillOrigin::Plugin { plugin } => {
            i18n::t_str("plugins-origin-plugin", &[("name", plugin)]).to_string()
        }
    };
    let edit_record = record.clone();
    let delete_name = record.name.clone();
    item_card(&theme, false)
        .child(
            h_flex()
                .justify_between()
                .gap_3()
                .child(item_text(record.key, origin, Some(source), &theme))
                .child(
                    h_flex()
                        .gap_1()
                        .child(
                            Button::new(format!("edit-skill-{}", delete_name))
                                .small()
                                .outline()
                                .label(if is_user {
                                    i18n::t("plugins-edit")
                                } else {
                                    i18n::t("plugins-view")
                                })
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.edit_skill(edit_record.clone(), window, cx);
                                })),
                        )
                        .children(is_user.then(|| {
                            Button::new(format!("delete-skill-{}", delete_name))
                                .small()
                                .danger()
                                .label(i18n::t("plugins-delete"))
                                .disabled(busy)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.delete_skill(delete_name.clone(), window, cx);
                                }))
                                .into_any_element()
                        })),
                ),
        )
        .into_any_element()
}

fn mcp_card(
    name: String,
    cfg: McpServerConfig,
    readonly: bool,
    busy: bool,
    _window: &mut Window,
    cx: &mut Context<PluginManagerView>,
) -> AnyElement {
    let theme = cx.theme().clone();
    let summary = mcp_summary(&cfg);
    let edit_name = name.clone();
    let edit_cfg = cfg.clone();
    let delete_name = name.clone();
    item_card(&theme, false)
        .child(
            h_flex()
                .justify_between()
                .gap_3()
                .child(item_text(name, summary, None, &theme))
                .children((!readonly).then(|| {
                    h_flex()
                        .gap_1()
                        .child(
                            Button::new(format!("edit-mcp-{}", edit_name))
                                .small()
                                .outline()
                                .label(i18n::t("plugins-edit"))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.edit_mcp(edit_name.clone(), edit_cfg.clone(), window, cx);
                                })),
                        )
                        .child(
                            Button::new(format!("delete-mcp-{}", delete_name))
                                .small()
                                .danger()
                                .label(i18n::t("plugins-delete"))
                                .disabled(busy)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.delete_mcp(delete_name.clone(), window, cx);
                                })),
                        )
                        .into_any_element()
                })),
        )
        .into_any_element()
}

fn plugin_mcp_card(record: mcp_config::PluginMcpServerRecord, theme: &Theme) -> AnyElement {
    item_card(theme, false)
        .child(item_text(
            format!("{}:{}", record.plugin, record.name),
            mcp_summary(&record.config),
            record.source.to_str().map(|s| s.to_string()),
            theme,
        ))
        .into_any_element()
}

fn form_card(
    title: SharedString,
    children: Vec<AnyElement>,
    cx: &mut Context<PluginManagerView>,
) -> AnyElement {
    let theme = cx.theme().clone();
    v_flex()
        .w_full()
        .gap_3()
        .p_3()
        .rounded(px(8.))
        .bg(theme.secondary.opacity(0.45))
        .child(
            div()
                .text_sm()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .child(title),
        )
        .children(children)
        .into_any_element()
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
                .child(title.into()),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(subtitle.into()),
        )
        .children(description.filter(|s| !s.is_empty()).map(|description| {
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(description)
        }))
        .into_any_element()
}

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

fn mcp_summary(cfg: &McpServerConfig) -> String {
    match &cfg.transport {
        McpServerTransportConfig::Stdio { command, args, .. } => {
            if args.is_empty() {
                command.clone()
            } else {
                format!("{} {}", command, args.join(" "))
            }
        }
        McpServerTransportConfig::StreamableHttp { url, .. } => url.clone(),
    }
}

fn split_args(raw: &str) -> Vec<String> {
    raw.split_whitespace().map(|s| s.to_string()).collect()
}
