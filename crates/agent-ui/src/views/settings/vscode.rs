//! Settings → External Tools → Visual Studio Code.app panel.
//!
//! Reads and edits the `vscode_app:` section of `cx.providers.config.yaml`
//! (`cx_providers::VsCodeAppSettings`). Two blocks — Claude Code Extension
//! and Codex Extension — each expose a Provider dropdown whose options are
//! the providers filtered by the extension's wire api (Anthropic vs
//! Responses, the latter shared with the ChatGPT.app catalog). Launching
//! VS Code (工具 menu / sidebar single entry) resolves injection from this
//! persisted config — no provider/model choice at launch time. The Codex
//! block reuses ChatGPT.app's CODEX_HOME + config.toml mechanism, so its
//! `env` / `supports_websockets` advanced settings stay in the ChatGPT.app
//! panel.
//!
//! Editable items autosave with the same debounced touch/save_generation
//! mechanism as the ChatGPT.app panel.

use std::sync::Arc;
use std::time::Duration;

use gpui::{
    Anchor, AnyElement, AnyWindowHandle, Context, Entity, IntoElement as _, ParentElement as _,
    SharedString, Styled as _, Window, div,
};
use gpui_component::theme::Theme;
use gpui_component::{
    ActiveTheme as _, WindowExt as _,
    button::Button,
    menu::{DropdownMenu as _, PopupMenuItem},
    notification::Notification,
    v_flex,
};

use crate::i18n;
use cx_providers::{VsCodeAppSettings, VsCodeExtensionBlock};

use super::SettingsView;
use super::chatgpt::{block, read_only_row};
use super::panels::{muted_text, panel_scroll, row_with_control};

const AUTOSAVE_DEBOUNCE_MS: u64 = 600;

/// Per-provider catalog entry: provider name → injectable model ids, or the
/// provider's load error (surfaced in the UI, never silently omitted).
type Catalog = Result<Vec<(String, Result<Vec<String>, String>)>, String>;

/// Which extension block a dropdown edit applies to.
#[derive(Clone, Copy)]
enum BlockKind {
    ClaudeCode,
    Codex,
}

type BlockApply =
    Arc<dyn Fn(&mut SettingsView, Option<String>, &mut Context<SettingsView>) + Send + Sync>;

pub struct VsCodePanelState {
    /// Config read failure message. Autosave stays disabled while set so an
    /// unrecognized file is never clobbered.
    load_error: Option<String>,
    claude_code: VsCodeExtensionBlock,
    codex: VsCodeExtensionBlock,
    /// Anthropic-wire catalog for the Claude Code block. `None` = loading.
    claude_catalog: Option<Catalog>,
    /// Responses-wire catalog for the Codex block. `None` = loading.
    codex_catalog: Option<Catalog>,
    /// Debounce token for autosave: each `touch` bumps it; a pending save
    /// task only writes when its captured generation is still current.
    save_generation: usize,
    window_handle: AnyWindowHandle,
}

impl VsCodePanelState {
    /// Build form state from the on-disk `vscode_app:` section and load both
    /// catalogs asynchronously (remote-models providers fetch online, so
    /// opening Settings must not block).
    pub fn load(window: &mut Window, cx: &mut Context<SettingsView>) -> Self {
        let mut state = Self {
            load_error: None,
            claude_code: VsCodeExtensionBlock::default(),
            codex: VsCodeExtensionBlock::default(),
            claude_catalog: None,
            codex_catalog: None,
            save_generation: 0,
            window_handle: window.window_handle(),
        };
        match cx::vscode_app_settings() {
            Ok(settings) => {
                state.claude_code = settings.claude_code;
                state.codex = settings.codex;
            }
            Err(e) => state.load_error = Some(format!("{e:#}")),
        }
        state.spawn_catalog_load(window, cx, BlockKind::ClaudeCode);
        state.spawn_catalog_load(window, cx, BlockKind::Codex);
        state
    }

    /// Load one catalog off the main thread. Tolerates the Settings overlay
    /// closing before the fetch completes (same `update_in` safety pattern as
    /// the ChatGPT.app panel).
    fn spawn_catalog_load(
        &self,
        window: &mut Window,
        cx: &mut Context<SettingsView>,
        kind: BlockKind,
    ) {
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    match kind {
                        BlockKind::ClaudeCode => cx::vscode_claude_injectable_catalog(),
                        BlockKind::Codex => cx::chatgpt_injectable_catalog(),
                    }
                })
                .await;
            let _ = this.update_in(cx, |this, _window, cx| {
                let mapped = match result {
                    Ok(catalog) => Ok(catalog),
                    Err(e) => Err(format!("{e:#}")),
                };
                match kind {
                    BlockKind::ClaudeCode => this.vscode_panel.claude_catalog = Some(mapped),
                    BlockKind::Codex => this.vscode_panel.codex_catalog = Some(mapped),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn catalog(&self, kind: BlockKind) -> &Option<Catalog> {
        match kind {
            BlockKind::ClaudeCode => &self.claude_catalog,
            BlockKind::Codex => &self.codex_catalog,
        }
    }

    /// Effective provider for a block: explicit selection wins; an unset
    /// block falls back to the catalog's first compatible provider (the same
    /// default the launch path resolves). `None` = effectively no injection.
    fn effective_provider(&self, kind: BlockKind) -> Option<String> {
        let block = match kind {
            BlockKind::ClaudeCode => &self.claude_code,
            BlockKind::Codex => &self.codex,
        };
        if block.disabled {
            return None;
        }
        if let Some(name) = &block.provider {
            return Some(name.clone());
        }
        first_ok_provider(self.catalog(kind))
    }

    /// Mark dirty and schedule a debounced autosave (same mechanism as the
    /// ChatGPT.app panel).
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
                this.vscode_panel.save_generation != generation
            });
            if stale {
                return;
            }
            let _ = handle.update(cx, |_view, window, cx| {
                entity.update(cx, |this, cx| {
                    if this.vscode_panel.save_generation == generation {
                        this.vscode_panel.save(window, cx);
                    }
                });
            });
        })
        .detach();
    }

    /// Write to disk. Failures surface as toasts while the form state is
    /// kept for correction.
    fn save(&mut self, window: &mut Window, cx: &mut Context<SettingsView>) {
        let settings = VsCodeAppSettings {
            claude_code: self.claude_code.clone(),
            codex: self.codex.clone(),
        };
        if let Err(e) = cx::save_vscode_app_settings(&settings) {
            tracing::warn!(error = %e, "failed to save vscode_app settings");
            window.push_notification(
                Notification::error(e.to_string()).title(i18n::t("settings-save-failed-title")),
                cx,
            );
        }
    }
}

/// First provider in catalog order whose entry loaded with a non-empty model
/// list — the launch-path default for unset blocks.
fn first_ok_provider(catalog: &Option<Catalog>) -> Option<String> {
    match catalog {
        Some(Ok(entries)) => entries.iter().find_map(|(name, entry)| match entry {
            Ok(models) if !models.is_empty() => Some(name.clone()),
            _ => None,
        }),
        _ => None,
    }
}

// --- Rendering ------------------------------------------------------------

pub fn render_vscode_app(view: &mut SettingsView, cx: &mut Context<SettingsView>) -> AnyElement {
    let theme = cx.theme().clone();
    let muted = theme.muted_foreground;

    let top_header = v_flex()
        .gap_1()
        .child(
            div()
                .text_xl()
                .font_weight(gpui::FontWeight::BLACK)
                .child(i18n::t("settings-panel-vscode-app")),
        )
        .child(muted_text(i18n::t("settings-desc-vscode-top"), muted))
        .into_any_element();

    if let Some(err) = view.vscode_panel.load_error.clone() {
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

    let claude_block = render_extension_block(view, &theme, cx, BlockKind::ClaudeCode);
    let codex_block = render_extension_block(view, &theme, cx, BlockKind::Codex);

    panel_scroll(
        v_flex()
            .w_full()
            .gap_6()
            .child(top_header)
            .child(claude_block)
            .child(codex_block),
    )
}

/// One extension block: Provider dropdown row + read-only Providers & LLMs
/// catalog rows (per-provider load failures render as such).
fn render_extension_block(
    view: &mut SettingsView,
    theme: &Theme,
    cx: &mut Context<SettingsView>,
    kind: BlockKind,
) -> AnyElement {
    let muted = theme.muted_foreground;
    let entity = cx.entity();

    let effective = view.vscode_panel.effective_provider(kind);
    let current_label = effective
        .clone()
        .map(SharedString::from)
        .unwrap_or_else(|| i18n::t("settings-vscode-no-inject"));

    // Dropdown options: 不注入 + catalog providers that loaded with models.
    let mut options: Vec<(SharedString, Option<String>)> =
        vec![(i18n::t("settings-vscode-no-inject"), None)];
    if let Some(Ok(entries)) = view.vscode_panel.catalog(kind) {
        for (name, entry) in entries {
            if let Ok(models) = entry
                && !models.is_empty()
            {
                options.push((SharedString::from(name.clone()), Some(name.clone())));
            }
        }
    }

    let apply: BlockApply = match kind {
        BlockKind::ClaudeCode => Arc::new(|this, token, cx| {
            apply_block(&mut this.vscode_panel.claude_code, token);
            this.vscode_panel.touch(cx);
        }),
        BlockKind::Codex => Arc::new(|this, token, cx| {
            apply_block(&mut this.vscode_panel.codex, token);
            this.vscode_panel.touch(cx);
        }),
    };
    let dropdown_id = match kind {
        BlockKind::ClaudeCode => "vscode-provider-claude-code",
        BlockKind::Codex => "vscode-provider-codex",
    };
    let dropdown = provider_dropdown(dropdown_id, current_label, options, entity, apply);

    let mut rows: Vec<AnyElement> = vec![row_with_control(
        i18n::t("settings-row-vscode-provider"),
        None,
        dropdown,
    )];

    // Read-only LLMs row: only for the provider currently selected in the
    // dropdown (explicit pick or the resolved default). 不注入 shows nothing.
    // The model list is already wire-api filtered by the catalog source
    // (Anthropic-wire VS Code models / Responses-wire ChatGPT.app models).
    if let Some(name) = effective.as_ref() {
        let empty_msg = match kind {
            BlockKind::ClaudeCode => i18n::t("settings-vscode-models-empty-anthropic"),
            BlockKind::Codex => i18n::t("settings-vscode-models-empty-responses"),
        };
        let label = SharedString::from(name.clone());
        match view.vscode_panel.catalog(kind) {
            None => rows.push(read_only_row(
                label,
                i18n::t("settings-vscode-models-loading"),
                muted,
            )),
            Some(Err(e)) => rows.push(read_only_row(
                label,
                i18n::t_str("settings-vscode-models-load-failed", &[("error", e)]),
                muted,
            )),
            Some(Ok(entries)) => match entries.iter().find(|(p, _)| p == name) {
                Some((_, Ok(models))) if models.is_empty() => {
                    rows.push(read_only_row(label, empty_msg, muted));
                }
                Some((_, Ok(models))) => rows.push(read_only_row(
                    label,
                    SharedString::from(models.join(", ")),
                    muted,
                )),
                Some((_, Err(e))) => rows.push(read_only_row(
                    label,
                    i18n::t_str("settings-vscode-models-load-failed", &[("error", e)]),
                    muted,
                )),
                // Provider no longer present in the catalog (stale explicit
                // selection); the launch path falls back with a warning.
                None => {}
            },
        }
    }

    let title = match kind {
        BlockKind::ClaudeCode => i18n::t("settings-section-vscode-claude"),
        BlockKind::Codex => i18n::t("settings-section-vscode-codex"),
    };
    block(title, None, theme, rows)
}

fn apply_block(block: &mut VsCodeExtensionBlock, token: Option<String>) {
    match token {
        None => {
            block.disabled = true;
            block.provider = None;
        }
        Some(name) => {
            block.disabled = false;
            block.provider = Some(name);
        }
    }
}

/// Provider dropdown: the selected option persists via `apply`; the label
/// shows the current effective provider (or 不注入).
fn provider_dropdown(
    id: impl Into<gpui::ElementId>,
    current_label: SharedString,
    options: Vec<(SharedString, Option<String>)>,
    entity: Entity<SettingsView>,
    apply: BlockApply,
) -> AnyElement {
    Button::new(id)
        .label(current_label)
        .dropdown_caret(true)
        .outline()
        .dropdown_menu_with_anchor(Anchor::BottomRight, move |menu, _window, _cx| {
            options.iter().fold(menu, |menu, (display, token)| {
                let token = token.clone();
                let entity = entity.clone();
                let apply = apply.clone();
                menu.item(
                    PopupMenuItem::new(display.clone()).on_click(move |_ev, _window, cx| {
                        entity.update(cx, |this, cx| {
                            apply(this, token.clone(), cx);
                            cx.notify();
                        });
                    }),
                )
            })
        })
        .into_any_element()
}
