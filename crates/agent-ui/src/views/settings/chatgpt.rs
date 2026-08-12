//! Settings → External Tools → ChatGPT.app panel.
//!
//! Displays and edits the ChatGPT.app injection settings
//! (`cx_providers::ChatGptAppSettings`, stored in the top-level `chatgpt_app:`
//! section of `cx.providers.config.yaml`, shared by the CLI and GUI launch
//! paths). The panel mirrors ChatGPT.app's own Settings visual language: a big
//! page heading, then per-block name + description above a border-only rounded
//! card whose rows are separated by hairlines (name in foreground color and
//! description in muted color on the left, value right-aligned). Blocks:
//! - Codex Home: read-only CODEX_HOME (copy / reveal in Finder);
//! - Model Injection: display nickname (optional) + injection mode (model list
//!   via CDP vs single model via the official config.toml mechanism) +
//!   read-only Providers & LLMs catalog;
//! - Variable Injection: custom environment variables (proxy vars implicitly);
//! - More Settings: `supports_websockets`.
//!
//! Editable items autosave with the same debounced touch/save_generation
//! mechanism as the Models panel. Launch args and the CDP script injection are
//! internal mechanics and are not surfaced here.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    AnyElement, AnyWindowHandle, App, AppContext as _, ClipboardItem, Context, Entity,
    IntoElement as _, ParentElement as _, SharedString, Styled as _, Window, div, px,
};
use gpui_component::theme::Theme;
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants},
    h_flex,
    input::{Input, InputEvent, InputState},
    notification::Notification,
    switch::Switch,
    v_flex,
};

use agent::i18n;
use cx_providers::{ChatGptAppSettings, ModelInjection};

use super::SettingsView;
use super::panels::{build_segmented_pair, hairline, muted_text, panel_scroll, row_with_control};

const AUTOSAVE_DEBOUNCE_MS: u64 = 600;
const KEY_W: f32 = 220.;

/// Injectable model catalog: provider name → injectable model ids, or a
/// per-provider load error surfaced in the UI (never silently omitted).
type Catalog = Result<Vec<(String, Result<Vec<String>, String>)>, String>;

pub struct ChatGptPanelState {
    /// Config read failure message. Autosave stays disabled while set so an
    /// unrecognized file is never clobbered.
    load_error: Option<String>,
    /// CODEX_HOME injected for ChatGPT.app (read-only display).
    codex_home: String,
    /// Injectable model catalog. `None` = still loading.
    catalog: Option<Catalog>,
    /// Nickname replacing the provider display name during injection
    /// (empty = keep the provider's own name).
    nickname: Entity<InputState>,
    /// Custom environment variable rows.
    env: Vec<EnvRow>,
    /// `supports_websockets`, default false (custom providers rarely support
    /// WebSocket streaming).
    supports_ws: bool,
    /// Injection mode: model list (CDP) / single model (official mechanism).
    model_injection: ModelInjection,
    /// Monotonic row id source so GPUI element ids stay unique across churn.
    next_row_id: usize,
    /// Debounce token for autosave: each `touch` bumps it; a pending save task
    /// only writes when its captured generation is still current.
    save_generation: usize,
    window_handle: AnyWindowHandle,
}

struct EnvRow {
    id: usize,
    key: Entity<InputState>,
    value: Entity<InputState>,
}

impl ChatGptPanelState {
    /// Build form state from the on-disk `chatgpt_app:` section and load the
    /// injectable catalog asynchronously (remote-models providers fetch online,
    /// so opening Settings must not block).
    pub fn load(window: &mut Window, cx: &mut Context<SettingsView>) -> Self {
        let codex_home = cx::chatgpt_codex_home()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "~/.config/cx/.codex".to_string());
        let mut state = Self {
            load_error: None,
            codex_home,
            catalog: None,
            nickname: panel_input(window, cx, "", i18n::t("settings-chatgpt-nickname-ph")),
            env: Vec::new(),
            supports_ws: false,
            model_injection: ModelInjection::default(),
            next_row_id: 1,
            save_generation: 0,
            window_handle: window.window_handle(),
        };
        match cx::chatgpt_app_settings() {
            Ok(settings) => state.apply_settings(window, cx, settings),
            Err(e) => state.load_error = Some(format!("{e:#}")),
        }
        state.spawn_catalog_load(window, cx);
        state
    }

    fn apply_settings(
        &mut self,
        window: &mut Window,
        cx: &mut Context<SettingsView>,
        settings: ChatGptAppSettings,
    ) {
        self.supports_ws = settings.supports_websockets.unwrap_or(false);
        self.model_injection = settings.model_injection;
        if let Some(nickname) = settings.nickname.as_deref() {
            self.nickname.update(cx, |state, cx| {
                state.set_value(nickname, window, cx);
            });
        }
        self.env = settings
            .env
            .into_iter()
            .map(|(k, v)| EnvRow::new(self.next_id(), window, cx, &k, &v))
            .collect();
    }

    fn next_id(&mut self) -> usize {
        let id = self.next_row_id;
        self.next_row_id += 1;
        id
    }

    /// Load the catalog off the main thread. Tolerates the Settings overlay
    /// closing before the fetch completes (same `update_in` safety pattern as
    /// the Models panel's registry reload): a dropped view yields `Err` from
    /// `update_in` instead of panicking.
    fn spawn_catalog_load(&self, window: &mut Window, cx: &mut Context<SettingsView>) {
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async { cx::chatgpt_injectable_catalog() })
                .await;
            let _ = this.update_in(cx, |this, _window, cx| {
                this.chatgpt_panel.catalog = Some(match result {
                    Ok(catalog) => Ok(catalog),
                    Err(e) => Err(format!("{e:#}")),
                });
                cx.notify();
            });
        })
        .detach();
    }

    /// Append an empty environment variable row.
    pub fn add_env_row(&mut self, window: &mut Window, cx: &mut Context<SettingsView>) {
        let id = self.next_id();
        self.env.push(EnvRow::new(id, window, cx, "", ""));
        if let Some(key) = self.env.last().map(|row| row.key.clone()) {
            key.update(cx, |state, cx| state.focus(window, cx));
        }
    }

    pub fn remove_env_row(&mut self, id: usize) {
        self.env.retain(|row| row.id != id);
    }

    /// Mark dirty and schedule a debounced autosave (same mechanism as the
    /// Models panel).
    pub fn touch(&mut self, cx: &mut Context<SettingsView>) {
        if self.load_error.is_some() {
            return;
        }
        self.save_generation = self.save_generation.wrapping_add(1);
        let generation = self.save_generation;
        let entity = cx.entity().clone();
        let handle = self.window_handle;
        cx.spawn(async move |_this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(AUTOSAVE_DEBOUNCE_MS))
                .await;
            let stale = entity.read_with(cx, |this, _| {
                this.chatgpt_panel.save_generation != generation
            });
            if stale {
                return;
            }
            let _ = handle.update(cx, |_view, window, cx| {
                entity.update(cx, |this, cx| {
                    if this.chatgpt_panel.save_generation == generation {
                        this.chatgpt_panel.save(window, cx);
                    }
                });
            });
        })
        .detach();
    }

    fn collect_env(&self, cx: &App) -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        for row in &self.env {
            let key = row.key.read(cx).value().trim().to_string();
            let value = row.value.read(cx).value().trim().to_string();
            if key.is_empty() {
                continue;
            }
            env.insert(key, value);
        }
        env
    }

    /// Validate and write to disk. Failures surface as toasts while the form
    /// state is kept for correction. A blank nickname is written as `None` so
    /// the untouched default stays omitted from the config file.
    fn save(&mut self, window: &mut Window, cx: &mut Context<SettingsView>) {
        let env = self.collect_env(cx);
        let nickname = self.nickname.read(cx).value().trim().to_string();
        if let Err(e) = cx::validate_chatgpt_custom_env(&env) {
            window.push_notification(
                Notification::error(e.to_string()).title(i18n::t("settings-save-failed-title")),
                cx,
            );
            return;
        }
        let settings = ChatGptAppSettings {
            env,
            nickname: (!nickname.is_empty()).then_some(nickname),
            supports_websockets: Some(self.supports_ws),
            model_injection: self.model_injection,
        };
        if let Err(e) = cx::save_chatgpt_app_settings(&settings) {
            tracing::warn!(error = %e, "failed to save chatgpt_app settings");
            window.push_notification(
                Notification::error(e.to_string()).title(i18n::t("settings-save-failed-title")),
                cx,
            );
        }
    }
}

impl EnvRow {
    fn new(
        id: usize,
        window: &mut Window,
        cx: &mut Context<SettingsView>,
        key: &str,
        value: &str,
    ) -> Self {
        Self {
            id,
            key: panel_input(window, cx, key, i18n::t("settings-chatgpt-env-key-ph")),
            value: panel_input(window, cx, value, i18n::t("settings-chatgpt-env-value-ph")),
        }
    }
}

/// One autosaving text input: edit events flow into the debounced save.
fn panel_input(
    window: &mut Window,
    cx: &mut Context<SettingsView>,
    value: &str,
    placeholder: SharedString,
) -> Entity<InputState> {
    let value = SharedString::from(value.to_string());
    let state = cx.new(|cx| {
        let mut state = InputState::new(window, cx).placeholder(placeholder);
        if !value.is_empty() {
            state.set_value(value, window, cx);
        }
        state
    });
    cx.subscribe(&state, |this, _state, event, cx| {
        if matches!(event, InputEvent::Change) {
            this.chatgpt_panel.touch(cx);
        }
    })
    .detach();
    state
}

// --- Rendering ------------------------------------------------------------

/// A settings block: name (+ description) left-aligned above a rounded
/// border-only card (no fill); rows inside are separated by hairlines.
pub(super) fn block(
    title: SharedString,
    desc: Option<SharedString>,
    theme: &Theme,
    rows: Vec<AnyElement>,
) -> AnyElement {
    let muted = theme.muted_foreground;
    let divider = theme.border.opacity(0.6);
    let mut card = v_flex()
        .w_full()
        .rounded(px(10.))
        .border_1()
        .border_color(divider);
    let last = rows.len().saturating_sub(1);
    for (ix, row) in rows.into_iter().enumerate() {
        card = card.child(row);
        if ix != last {
            card = card.child(hairline(divider));
        }
    }
    let mut head = v_flex().w_full().gap_1().child(
        div()
            .px_1()
            .text_sm()
            .font_weight(gpui::FontWeight::NORMAL)
            .text_color(theme.foreground)
            .child(title),
    );
    if let Some(desc) = desc {
        head = head.child(
            div()
                .w_full()
                .px_1()
                .text_xs()
                .font_weight(gpui::FontWeight::NORMAL)
                .text_color(muted)
                .child(desc),
        );
    }
    v_flex()
        .w_full()
        .gap_2()
        .child(head)
        .child(card)
        .into_any_element()
}

/// Read-only two-line row: name (foreground) + description (muted), no value.
pub(super) fn read_only_row(
    title: SharedString,
    desc: SharedString,
    muted: gpui::Hsla,
) -> AnyElement {
    h_flex()
        .w_full()
        .items_start()
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
                        .child(title),
                )
                .child(
                    div()
                        .w_full()
                        .text_xs()
                        .font_weight(gpui::FontWeight::NORMAL)
                        .text_color(muted)
                        .child(desc),
                ),
        )
        .into_any_element()
}

pub fn render_chatgpt_app(view: &mut SettingsView, cx: &mut Context<SettingsView>) -> AnyElement {
    let theme = cx.theme().clone();
    let muted = theme.muted_foreground;

    // Big heading carries the page theme (the TitleBar text was removed).
    let top_header = v_flex()
        .gap_1()
        .child(
            div()
                .text_xl()
                .font_weight(gpui::FontWeight::BLACK)
                .child(i18n::t("settings-panel-chatgpt-app")),
        )
        .child(muted_text(i18n::t("settings-desc-chatgpt-top"), muted))
        .into_any_element();

    if let Some(err) = view.chatgpt_panel.load_error.clone() {
        return panel_scroll(
            v_flex().w_full().gap_4().child(top_header).child(
                div()
                    .w_full()
                    .text_xs()
                    .text_color(theme.danger)
                    .child(SharedString::from(err)),
            ),
        );
    }

    let home_block = render_home_block(view, &theme);
    let injection_block = render_injection_block(view, &theme, cx);
    let env_block = render_env_block(view, &theme, cx);
    let more_block = render_more_block(view, &theme, cx);

    panel_scroll(
        v_flex()
            .w_full()
            .gap_6()
            .child(top_header)
            .child(home_block)
            .child(injection_block)
            .child(env_block)
            .child(more_block),
    )
}

/// Codex Home: the CODEX_HOME env var and its value (read-only, with copy /
/// reveal-in-Finder actions).
fn render_home_block(view: &mut SettingsView, theme: &Theme) -> AnyElement {
    let muted = theme.muted_foreground;
    let codex_home: SharedString = view.chatgpt_panel.codex_home.clone().into();
    let home_for_reveal = view.chatgpt_panel.codex_home.clone();
    let home_for_copy = codex_home.clone();

    let rows = vec![row_with_control(
        SharedString::from("CODEX_HOME"),
        None,
        h_flex()
            .gap_2()
            .items_center()
            .child(div().text_xs().text_color(muted).child(codex_home))
            .child(
                Button::new("chatgpt-copy-home")
                    .label(i18n::t("settings-btn-copy"))
                    .outline()
                    .small()
                    .on_click(move |_ev, _window, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(home_for_copy.to_string()));
                    }),
            )
            .child(
                Button::new("chatgpt-reveal-home")
                    .label(i18n::t("settings-btn-reveal"))
                    .outline()
                    .small()
                    .on_click(move |_ev, _window, _cx| {
                        let path = home_for_reveal.clone();
                        std::thread::spawn(move || {
                            if let Err(e) = open::that(path) {
                                tracing::warn!(error = %e, "reveal CODEX_HOME failed");
                            }
                        });
                    }),
            )
            .into_any_element(),
    )];
    block(
        i18n::t("settings-section-chatgpt-home"),
        Some(i18n::t("settings-desc-chatgpt-home")),
        theme,
        rows,
    )
}

/// Model Injection: item 1 = display nickname (replaces the provider name
/// during injection when set); item 2 = injection mode (segmented two-choice;
/// the CDP risk note is the row's inline description); item 3 = Providers &
/// LLMs read-only catalog, one two-line row per provider. Per-provider fetch
/// failures render as a "failed to load" row instead of vanishing.
fn render_injection_block(
    view: &mut SettingsView,
    theme: &Theme,
    cx: &mut Context<SettingsView>,
) -> AnyElement {
    let muted = theme.muted_foreground;
    let entity = cx.entity();
    let current = view.chatgpt_panel.model_injection;

    let mut rows = vec![
        row_with_control(
            i18n::t("settings-row-chatgpt-nickname"),
            Some(muted_text(i18n::t("settings-desc-chatgpt-nickname"), muted)),
            Input::new(&view.chatgpt_panel.nickname)
                .w(px(240.))
                .into_any_element(),
        ),
        row_with_control(
            i18n::t("settings-row-chatgpt-injection"),
            Some(muted_text(
                i18n::t("settings-desc-chatgpt-injection-risk"),
                muted,
            )),
            build_segmented_pair(
                "chatgpt-injection",
                current == ModelInjection::List,
                (
                    i18n::t("settings-value-injection-list"),
                    Arc::new(|this, cx| {
                        this.chatgpt_panel.model_injection = ModelInjection::List;
                        this.chatgpt_panel.touch(cx);
                    }),
                ),
                (
                    i18n::t("settings-value-injection-single"),
                    Arc::new(|this, cx| {
                        this.chatgpt_panel.model_injection = ModelInjection::Single;
                        this.chatgpt_panel.touch(cx);
                    }),
                ),
                theme,
                entity.clone(),
            ),
        ),
    ];

    match &view.chatgpt_panel.catalog {
        None => rows.push(read_only_row(
            i18n::t("settings-row-chatgpt-providers"),
            i18n::t("settings-chatgpt-models-loading"),
            muted,
        )),
        Some(Err(e)) => rows.push(read_only_row(
            i18n::t("settings-row-chatgpt-providers"),
            SharedString::from(e.clone()),
            muted,
        )),
        Some(Ok(entries)) if entries.is_empty() => rows.push(read_only_row(
            i18n::t("settings-row-chatgpt-providers"),
            i18n::t("settings-chatgpt-models-empty"),
            muted,
        )),
        Some(Ok(entries)) => {
            for (provider, entry) in entries {
                let provider_label = SharedString::from(provider.clone());
                let desc = match entry {
                    Ok(models) if models.is_empty() => i18n::t("settings-chatgpt-models-empty"),
                    Ok(models) => SharedString::from(models.join(", ")),
                    Err(e) => i18n::t_str("settings-chatgpt-models-load-failed", &[("error", e)]),
                };
                rows.push(read_only_row(provider_label, desc, muted));
            }
        }
    }

    block(
        i18n::t("settings-section-chatgpt-injection"),
        Some(i18n::t("settings-desc-chatgpt-injection")),
        theme,
        rows,
    )
}

/// Variable Injection: custom environment variable key/value rows + add/remove.
fn render_env_block(
    view: &mut SettingsView,
    theme: &Theme,
    cx: &mut Context<SettingsView>,
) -> AnyElement {
    let entity = cx.entity();
    let rows_src: Vec<(usize, Entity<InputState>, Entity<InputState>)> = view
        .chatgpt_panel
        .env
        .iter()
        .map(|r| (r.id, r.key.clone(), r.value.clone()))
        .collect();

    let mut rows: Vec<AnyElement> = Vec::new();
    for (row_id, key, value) in rows_src {
        let entity_del = entity.clone();
        rows.push(
            h_flex()
                .px_3()
                .py_2()
                .items_center()
                .gap_2()
                .child(div().w(px(KEY_W)).flex_shrink_0().child(Input::new(&key)))
                .child(div().flex_1().min_w_0().child(Input::new(&value)))
                .child(
                    Button::new(format!("chatgpt-env-del-{row_id}"))
                        .icon(Icon::new(IconName::Minus))
                        .ghost()
                        .small()
                        .on_click(move |_ev, _window, cx| {
                            entity_del.update(cx, |this, cx| {
                                this.chatgpt_panel.remove_env_row(row_id);
                                this.chatgpt_panel.touch(cx);
                                cx.notify();
                            });
                        }),
                )
                .into_any_element(),
        );
    }
    rows.push(
        h_flex()
            .px_3()
            .py_2()
            .child(
                Button::new("chatgpt-env-add")
                    .label(i18n::t("settings-btn-add-env"))
                    .icon(Icon::new(IconName::Plus))
                    .outline()
                    .small()
                    .on_click(move |_ev, window, cx| {
                        entity.update(cx, |this, cx| {
                            this.chatgpt_panel.add_env_row(window, cx);
                            cx.notify();
                        });
                    }),
            )
            .into_any_element(),
    );

    block(
        i18n::t("settings-section-chatgpt-env"),
        Some(i18n::t("settings-desc-chatgpt-env")),
        theme,
        rows,
    )
}

/// More Settings: advanced options written into the injected config.toml
/// (currently only `supports_websockets`).
fn render_more_block(
    view: &mut SettingsView,
    theme: &Theme,
    cx: &mut Context<SettingsView>,
) -> AnyElement {
    let muted = theme.muted_foreground;
    let entity = cx.entity();
    let checked = view.chatgpt_panel.supports_ws;

    let rows = vec![row_with_control(
        SharedString::from("supports_websockets"),
        Some(muted_text(
            i18n::t("settings-desc-chatgpt-websockets"),
            muted,
        )),
        Switch::new("chatgpt-ws-toggle")
            .checked(checked)
            .on_click(move |&new_val, _window, cx| {
                entity.update(cx, |this, cx| {
                    this.chatgpt_panel.supports_ws = new_val;
                    this.chatgpt_panel.touch(cx);
                    cx.notify();
                });
            })
            .into_any_element(),
    )];
    block(
        i18n::t("settings-section-chatgpt-more"),
        Some(i18n::t("settings-desc-chatgpt-more")),
        theme,
        rows,
    )
}
