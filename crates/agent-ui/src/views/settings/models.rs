//! Settings → General → Models panel.
//!
//! Form-based view over the cx providers config
//! (`~/.config/cx/cx.providers.config.yaml`, schema: `cx_providers::CxConfig`).
//! Opening the panel parses the file into per-field input state (回显); the
//! Save button validates the form, atomically writes the whole config back —
//! preserving the top-level `agents:` section verbatim — and reloads the
//! process-wide provider registry on a background thread so new threads pick
//! up the change.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use gpui::{
    Anchor, AnyElement, App, AppContext as _, Context, Entity, InteractiveElement as _,
    IntoElement as _, ParentElement as _, SharedString, StatefulInteractiveElement as _,
    Styled as _, Window, div, px,
};
use gpui_component::theme::Theme;
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants},
    checkbox::Checkbox,
    h_flex,
    input::{Input, InputState},
    menu::{DropdownMenu as _, PopupMenuItem},
    notification::Notification,
    v_flex,
};

use agent::i18n;
use cx_providers::{
    AgentConfig, CxConfig, ProviderConfig, ProviderEndpointDetail, ProviderEndpointSpec,
    ProviderModelConfig, ProviderModels, active_provider_config_path, read_config_file,
    write_config_file,
};

use super::SettingsView;
use super::panels::{hairline, muted_text, panel_scroll, section_card, section_header};

const LABEL_W: f32 = 150.;
const KEY_W: f32 = 220.;
const WIRE_APIS: [&str; 3] = ["anthropic", "responses", "completions"];

// --- Panel state ----------------------------------------------------------

/// Everything the Models panel knows about the config file. Built once when
/// the Settings view is created; `Reload` rebuilds it from disk, `Save`
/// serializes it back.
pub struct ModelsPanelState {
    /// Resolved config file path (`None` only when `$HOME` is unresolvable).
    path: Option<PathBuf>,
    /// Read/parse failure message. Save stays disabled while set so an
    /// unrecognized file is never clobbered.
    load_error: Option<String>,
    /// Top-level `agents:` section — out of scope for this form, preserved
    /// verbatim across saves.
    agents: Vec<AgentConfig>,
    providers: Vec<ProviderForm>,
    /// Provider form ids whose card body is expanded.
    expanded: HashSet<usize>,
    /// Monotonic id source so GPUI element ids stay unique across
    /// add/remove churn.
    next_form_id: usize,
}

struct ProviderForm {
    id: usize,
    name: Entity<InputState>,
    apikey_source: Entity<InputState>,
    endpoints: Vec<EndpointForm>,
    env: Vec<KvForm>,
    /// `models:` is a remote URL string instead of inline definitions.
    remote_models: bool,
    remote_url: Entity<InputState>,
    models: Vec<ModelForm>,
}

struct EndpointForm {
    id: usize,
    /// Map key in `endpoints:` — one of `anthropic` / `responses` /
    /// `completions`.
    wire_api: SharedString,
    url: Entity<InputState>,
    /// Comma-separated `agents:` list.
    agents: Entity<InputState>,
    /// Empty = unset (null in YAML), else `api_key` / `bearer_token`.
    copilot_auth: SharedString,
}

struct KvForm {
    id: usize,
    key: Entity<InputState>,
    value: Entity<InputState>,
}

struct ModelForm {
    id: usize,
    model_id: Entity<InputState>,
    desc: Entity<InputState>,
    context: Entity<InputState>,
    max_tokens: Entity<InputState>,
    wire_anthropic: bool,
    wire_responses: bool,
    wire_completions: bool,
    /// Comma-separated `agents:` list.
    agents: Entity<InputState>,
    env: Vec<KvForm>,
    /// Empty = unset (null in YAML), else `true` / `false`.
    supports_tools: SharedString,
    supports_images: SharedString,
}

type MutApply = Arc<dyn Fn(&mut SettingsView) + Send + Sync + 'static>;
type WindowApply =
    Arc<dyn Fn(&mut SettingsView, &mut Window, &mut Context<SettingsView>) + Send + Sync + 'static>;
type TokenApply = Arc<dyn Fn(&mut SettingsView, SharedString) + Send + Sync + 'static>;

impl ModelsPanelState {
    /// Read the active config file and build the form state. A missing file
    /// is not an error — saving creates it.
    pub fn load(window: &mut Window, cx: &mut Context<SettingsView>) -> Self {
        let mut state = Self {
            path: None,
            load_error: None,
            agents: Vec::new(),
            providers: Vec::new(),
            expanded: HashSet::new(),
            next_form_id: 1,
        };
        state.reload_from_disk(window, cx);
        state
    }

    /// Re-read the config file, discarding any unsaved edits.
    pub fn reload_from_disk(&mut self, window: &mut Window, cx: &mut Context<SettingsView>) {
        self.agents.clear();
        self.providers.clear();
        self.expanded.clear();
        self.load_error = None;

        let path = match active_provider_config_path() {
            Ok(path) => path,
            Err(e) => {
                self.path = None;
                self.load_error = Some(format!("{e:#}"));
                return;
            }
        };
        self.path = Some(path.clone());
        if !path.exists() {
            return;
        }
        match read_config_file(&path) {
            Ok(config) => self.apply_config(window, cx, config),
            Err(e) => self.load_error = Some(format!("{e:#}")),
        }
    }

    fn apply_config(
        &mut self,
        window: &mut Window,
        cx: &mut Context<SettingsView>,
        config: CxConfig,
    ) {
        self.agents = config.agents;
        self.providers = config
            .providers
            .into_iter()
            .map(|p| ProviderForm::from_config(p, window, cx, &mut self.next_form_id))
            .collect();
        // Echo everything expanded so the file contents are visible at a glance.
        self.expanded = self.providers.iter().map(|p| p.id).collect();
        self.load_error = None;
    }

    fn new_form_id(&mut self) -> usize {
        let id = self.next_form_id;
        self.next_form_id += 1;
        id
    }

    fn provider_mut(&mut self, id: usize) -> Option<&mut ProviderForm> {
        self.providers.iter_mut().find(|p| p.id == id)
    }

    fn endpoint_mut(&mut self, provider_id: usize, id: usize) -> Option<&mut EndpointForm> {
        self.provider_mut(provider_id)?
            .endpoints
            .iter_mut()
            .find(|e| e.id == id)
    }

    fn model_mut(&mut self, provider_id: usize, id: usize) -> Option<&mut ModelForm> {
        self.provider_mut(provider_id)?
            .models
            .iter_mut()
            .find(|m| m.id == id)
    }

    fn toggle_expanded(&mut self, id: usize) {
        if !self.expanded.remove(&id) {
            self.expanded.insert(id);
        }
    }

    fn add_provider(&mut self, window: &mut Window, cx: &mut Context<SettingsView>) {
        let id = self.new_form_id();
        let form = ProviderForm {
            id,
            name: new_input(window, cx, "", Some("settings-models-ph-name")),
            apikey_source: new_input(window, cx, "", Some("settings-models-ph-apikey")),
            endpoints: Vec::new(),
            env: Vec::new(),
            remote_models: false,
            remote_url: new_input(window, cx, "", Some("settings-models-ph-remote-url")),
            models: Vec::new(),
        };
        self.providers.push(form);
        self.expanded.insert(id);
    }

    fn remove_provider(&mut self, id: usize) {
        self.providers.retain(|p| p.id != id);
        self.expanded.remove(&id);
    }

    fn add_endpoint(
        &mut self,
        window: &mut Window,
        cx: &mut Context<SettingsView>,
        provider_id: usize,
    ) {
        let id = self.new_form_id();
        if let Some(provider) = self.provider_mut(provider_id) {
            provider.endpoints.push(EndpointForm {
                id,
                wire_api: SharedString::from("anthropic"),
                url: new_input(window, cx, "", Some("settings-models-ph-url")),
                agents: new_input(window, cx, "", Some("settings-models-ph-agents")),
                copilot_auth: SharedString::default(),
            });
        }
    }

    fn remove_endpoint(&mut self, provider_id: usize, id: usize) {
        if let Some(provider) = self.provider_mut(provider_id) {
            provider.endpoints.retain(|e| e.id != id);
        }
    }

    fn add_provider_env(
        &mut self,
        window: &mut Window,
        cx: &mut Context<SettingsView>,
        provider_id: usize,
    ) {
        let id = self.new_form_id();
        if let Some(provider) = self.provider_mut(provider_id) {
            provider.env.push(KvForm::new(id, window, cx));
        }
    }

    fn add_model(
        &mut self,
        window: &mut Window,
        cx: &mut Context<SettingsView>,
        provider_id: usize,
    ) {
        let id = self.new_form_id();
        if let Some(provider) = self.provider_mut(provider_id) {
            provider.models.push(ModelForm {
                id,
                model_id: new_input(window, cx, "", None),
                desc: new_input(window, cx, "", None),
                context: new_input(window, cx, "", Some("settings-models-ph-context")),
                max_tokens: new_input(window, cx, "", Some("settings-models-ph-max-tokens")),
                wire_anthropic: true,
                wire_responses: false,
                wire_completions: false,
                agents: new_input(window, cx, "", Some("settings-models-ph-agents")),
                env: Vec::new(),
                supports_tools: SharedString::default(),
                supports_images: SharedString::default(),
            });
        }
    }

    fn remove_model(&mut self, provider_id: usize, id: usize) {
        if let Some(provider) = self.provider_mut(provider_id) {
            provider.models.retain(|m| m.id != id);
        }
    }

    fn add_model_env(
        &mut self,
        window: &mut Window,
        cx: &mut Context<SettingsView>,
        provider_id: usize,
        model_id: usize,
    ) {
        let id = self.new_form_id();
        if let Some(model) = self.model_mut(provider_id, model_id) {
            model.env.push(KvForm::new(id, window, cx));
        }
    }

    /// Validate the form and rebuild the `CxConfig`. Errors are localized
    /// single-line messages suitable for a toast.
    fn collect(&self, cx: &App) -> Result<CxConfig, SharedString> {
        let mut providers = Vec::with_capacity(self.providers.len());
        for (index, p) in self.providers.iter().enumerate() {
            let name = p.name.read(cx).value().trim().to_string();
            if name.is_empty() {
                return Err(i18n::t_str(
                    "settings-models-err-provider-name",
                    &[("index", &index.to_string())],
                ));
            }
            let apikey_source = optional_text(&p.apikey_source.read(cx).value());

            let mut endpoints = BTreeMap::new();
            for e in &p.endpoints {
                let url = e.url.read(cx).value().trim().to_string();
                if url.is_empty() {
                    return Err(provider_err(&name, "settings-models-err-endpoint-url", &[]));
                }
                let wire = e.wire_api.to_string();
                if endpoints.contains_key(&wire) {
                    return Err(provider_err(
                        &name,
                        "settings-models-err-endpoint-dup",
                        &[("wire", &wire)],
                    ));
                }
                let agents = comma_list(&e.agents.read(cx).value());
                let copilot_auth = optional_text(&e.copilot_auth);
                let spec = if agents.is_empty() && copilot_auth.is_none() {
                    ProviderEndpointSpec::Url(url)
                } else {
                    ProviderEndpointSpec::Detailed(ProviderEndpointDetail {
                        url,
                        agents,
                        copilot_auth,
                    })
                };
                endpoints.insert(wire, spec);
            }

            let env = collect_env(cx, &p.env, &name, None)?;

            let models = if p.remote_models {
                let url = p.remote_url.read(cx).value().trim().to_string();
                if url.is_empty() {
                    return Err(provider_err(&name, "settings-models-err-remote-url", &[]));
                }
                ProviderModels::RemoteUrl(url)
            } else {
                let mut map = BTreeMap::new();
                for m in &p.models {
                    let id = m.model_id.read(cx).value().trim().to_string();
                    if id.is_empty() {
                        return Err(provider_err(&name, "settings-models-err-model-id", &[]));
                    }
                    if map.contains_key(&id) {
                        return Err(provider_err(
                            &name,
                            "settings-models-err-model-dup",
                            &[("id", &id)],
                        ));
                    }
                    let context = parse_opt_u64(&m.context.read(cx).value()).map_err(|()| {
                        model_err(
                            &name,
                            &id,
                            "settings-models-err-number",
                            &[("field", i18n::t("settings-models-row-context").as_ref())],
                        )
                    })?;
                    let max_tokens =
                        parse_opt_u64(&m.max_tokens.read(cx).value()).map_err(|()| {
                            model_err(
                                &name,
                                &id,
                                "settings-models-err-number",
                                &[("field", i18n::t("settings-models-row-max-tokens").as_ref())],
                            )
                        })?;
                    let mut wire_apis = Vec::new();
                    if m.wire_anthropic {
                        wire_apis.push("anthropic".to_string());
                    }
                    if m.wire_responses {
                        wire_apis.push("responses".to_string());
                    }
                    if m.wire_completions {
                        wire_apis.push("completions".to_string());
                    }
                    map.insert(
                        id.clone(),
                        ProviderModelConfig {
                            desc: optional_text(&m.desc.read(cx).value()),
                            wire_apis,
                            agents: comma_list(&m.agents.read(cx).value()),
                            env: collect_env(cx, &m.env, &name, Some(&id))?,
                            max_tokens,
                            context,
                            supports_tools: parse_opt_bool(&m.supports_tools),
                            supports_images: parse_opt_bool(&m.supports_images),
                        },
                    );
                }
                ProviderModels::Inline(map)
            };

            providers.push(ProviderConfig {
                name,
                apikey_source,
                models,
                endpoints,
                env,
            });
        }
        Ok(CxConfig {
            providers,
            agents: self.agents.clone(),
        })
    }

    /// Validate, write the config file, and refresh the runtime registry.
    pub fn save(&mut self, window: &mut Window, cx: &mut Context<SettingsView>) {
        if self.load_error.is_some() {
            return;
        }
        let Some(path) = self.path.clone() else {
            window.push_notification(
                Notification::error(i18n::t("settings-models-no-path"))
                    .title(i18n::t("settings-save-failed-title")),
                cx,
            );
            return;
        };
        let config = match self.collect(cx) {
            Ok(config) => config,
            Err(msg) => {
                window.push_notification(
                    Notification::error(msg).title(i18n::t("settings-save-failed-title")),
                    cx,
                );
                return;
            }
        };
        if let Err(e) = write_config_file(&path, &config) {
            tracing::warn!(error = %e, "failed to write cx providers config");
            window.push_notification(
                Notification::error(e.to_string()).title(i18n::t("settings-save-failed-title")),
                cx,
            );
            return;
        }
        window.push_notification(Notification::success(i18n::t("settings-models-saved")), cx);

        // Rebuild the process-wide registry from the freshly written file so
        // new threads see the change without a restart. Api key resolution
        // may hit the OS keychain or run shell commands, so it runs off the
        // main thread; on failure the previous registry stays live.
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { agent::pi_providers::reload() })
                .await;
            let _ = this.update_in(cx, |_this, window, cx| match result {
                Ok(()) => i18n::rebuild_menus(cx),
                Err(e) => {
                    tracing::warn!(error = %e, "provider registry reload failed after settings save");
                    window.push_notification(
                        Notification::error(e.to_string())
                            .title(i18n::t("settings-models-reload-failed-title")),
                        cx,
                    );
                }
            });
        })
        .detach();
    }
}

impl KvForm {
    fn new(id: usize, window: &mut Window, cx: &mut Context<SettingsView>) -> Self {
        Self {
            id,
            key: new_input(window, cx, "", None),
            value: new_input(window, cx, "", None),
        }
    }
}

impl ProviderForm {
    fn from_config(
        config: ProviderConfig,
        window: &mut Window,
        cx: &mut Context<SettingsView>,
        next_form_id: &mut usize,
    ) -> Self {
        let mut take_id = || {
            let id = *next_form_id;
            *next_form_id += 1;
            id
        };
        let endpoints = config
            .endpoints
            .into_iter()
            .map(|(wire_api, spec)| {
                let (url, agents, copilot_auth) = match spec {
                    ProviderEndpointSpec::Url(url) => (url, Vec::new(), None),
                    ProviderEndpointSpec::Detailed(detail) => {
                        (detail.url, detail.agents, detail.copilot_auth)
                    }
                };
                EndpointForm {
                    id: take_id(),
                    wire_api: SharedString::from(wire_api),
                    url: new_input(window, cx, &url, Some("settings-models-ph-url")),
                    agents: new_input(
                        window,
                        cx,
                        &agents.join(", "),
                        Some("settings-models-ph-agents"),
                    ),
                    copilot_auth: SharedString::from(copilot_auth.unwrap_or_default()),
                }
            })
            .collect();
        let env = config
            .env
            .into_iter()
            .map(|(key, value)| KvForm {
                id: take_id(),
                key: new_input(window, cx, &key, None),
                value: new_input(window, cx, &value, None),
            })
            .collect();
        let (remote_models, remote_url, models) = match config.models {
            ProviderModels::RemoteUrl(url) => (
                true,
                new_input(window, cx, &url, Some("settings-models-ph-remote-url")),
                Vec::new(),
            ),
            ProviderModels::Inline(map) => {
                let models = map
                    .into_iter()
                    .map(|(id, model)| ModelForm {
                        id: take_id(),
                        model_id: new_input(window, cx, &id, None),
                        desc: new_input(
                            window,
                            cx,
                            model.desc.as_deref().unwrap_or_default(),
                            None,
                        ),
                        context: new_input(
                            window,
                            cx,
                            model
                                .context
                                .map(|v| v.to_string())
                                .as_deref()
                                .unwrap_or_default(),
                            Some("settings-models-ph-context"),
                        ),
                        max_tokens: new_input(
                            window,
                            cx,
                            model
                                .max_tokens
                                .map(|v| v.to_string())
                                .as_deref()
                                .unwrap_or_default(),
                            Some("settings-models-ph-max-tokens"),
                        ),
                        wire_anthropic: model.wire_apis.iter().any(|w| w == "anthropic"),
                        wire_responses: model.wire_apis.iter().any(|w| w == "responses"),
                        wire_completions: model.wire_apis.iter().any(|w| w == "completions"),
                        agents: new_input(
                            window,
                            cx,
                            &model.agents.join(", "),
                            Some("settings-models-ph-agents"),
                        ),
                        env: model
                            .env
                            .into_iter()
                            .map(|(key, value)| KvForm {
                                id: take_id(),
                                key: new_input(window, cx, &key, None),
                                value: new_input(window, cx, &value, None),
                            })
                            .collect(),
                        supports_tools: SharedString::from(
                            model
                                .supports_tools
                                .map(|v| v.to_string())
                                .unwrap_or_default(),
                        ),
                        supports_images: SharedString::from(
                            model
                                .supports_images
                                .map(|v| v.to_string())
                                .unwrap_or_default(),
                        ),
                    })
                    .collect();
                (
                    false,
                    new_input(window, cx, "", Some("settings-models-ph-remote-url")),
                    models,
                )
            }
        };
        Self {
            id: take_id(),
            name: new_input(window, cx, &config.name, Some("settings-models-ph-name")),
            apikey_source: new_input(
                window,
                cx,
                config.apikey_source.as_deref().unwrap_or_default(),
                Some("settings-models-ph-apikey"),
            ),
            endpoints,
            env,
            remote_models,
            remote_url,
            models,
        }
    }
}

// --- Small helpers --------------------------------------------------------

fn new_input(
    window: &mut Window,
    cx: &mut Context<SettingsView>,
    value: &str,
    placeholder_key: Option<&'static str>,
) -> Entity<InputState> {
    let value = SharedString::from(value.to_string());
    cx.new(|cx| {
        let mut state = match placeholder_key {
            Some(key) => InputState::new(window, cx).placeholder(i18n::t(key)),
            None => InputState::new(window, cx),
        };
        if !value.is_empty() {
            state.set_value(value, window, cx);
        }
        state
    })
}

fn optional_text(value: &SharedString) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn comma_list(value: &SharedString) -> Vec<String> {
    value
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Empty stays `None` (null in YAML); anything else is the literal token
/// (`true` / `false`, `api_key` / `bearer_token`).
fn parse_opt_bool(value: &SharedString) -> Option<bool> {
    match value.trim() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

fn parse_opt_u64(value: &SharedString) -> Result<Option<u64>, ()> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    trimmed.parse::<u64>().map(Some).map_err(|_| ())
}

/// Collect `KEY: value` rows. Blank rows are skipped; a value without a key
/// is an error. `model_id` narrows the error message to a model-level `env`
/// block.
fn collect_env(
    cx: &App,
    rows: &[KvForm],
    provider_name: &str,
    model_id: Option<&str>,
) -> Result<BTreeMap<String, String>, SharedString> {
    let mut env = BTreeMap::new();
    for kv in rows {
        let key = kv.key.read(cx).value().trim().to_string();
        // Trim symmetrically with the key: stray padding around env values is
        // never intentional and would silently defeat exact-match parsing
        // downstream (e.g. `MANOX_PROMPT_CACHING: " full"`).
        let value = kv.value.read(cx).value().trim().to_string();
        if key.is_empty() {
            if value.is_empty() {
                continue;
            }
            return Err(match model_id {
                Some(id) => model_err(provider_name, id, "settings-models-err-env-key", &[]),
                None => provider_err(provider_name, "settings-models-err-env-key", &[]),
            });
        }
        env.insert(key, value);
    }
    Ok(env)
}

/// Localized validation error scoped to a provider.
fn provider_err(provider_name: &str, key: &'static str, extra: &[(&str, &str)]) -> SharedString {
    let mut args: Vec<(&str, &str)> = Vec::with_capacity(extra.len() + 1);
    args.push(("name", provider_name));
    args.extend_from_slice(extra);
    i18n::t_str(key, &args)
}

/// Localized validation error scoped to a model inside a provider.
fn model_err(
    provider_name: &str,
    model_id: &str,
    key: &'static str,
    extra: &[(&str, &str)],
) -> SharedString {
    let mut args: Vec<(&str, &str)> = Vec::with_capacity(extra.len() + 2);
    args.push(("name", provider_name));
    args.push(("id", model_id));
    args.extend_from_slice(extra);
    i18n::t_str(key, &args)
}

// --- Rendering ------------------------------------------------------------

/// Right-pane renderer for Settings → General → Models.
pub fn render_models(view: &mut SettingsView, cx: &mut Context<SettingsView>) -> AnyElement {
    let theme = cx.theme().clone();
    let entity = cx.entity();

    let mut children: Vec<AnyElement> = vec![render_header(view, &theme, entity.clone())];

    if let Some(err) = view.models_panel.load_error.clone() {
        children.push(render_load_error(&theme, err));
    } else if view.models_panel.providers.is_empty() {
        children.push(render_empty(&theme, entity.clone()));
    }

    let provider_ids: Vec<usize> = view.models_panel.providers.iter().map(|p| p.id).collect();
    for pid in provider_ids {
        children.push(render_provider(view, &theme, entity.clone(), pid, cx));
    }

    panel_scroll(v_flex().w_full().gap_4().children(children))
}

fn render_header(
    view: &mut SettingsView,
    theme: &Theme,
    entity: Entity<SettingsView>,
) -> AnyElement {
    let muted = theme.muted_foreground;
    let panel = &view.models_panel;
    let path_caption = match &panel.path {
        Some(path) => {
            let path = path.display().to_string();
            i18n::t_str("settings-models-file-caption", &[("path", &path)])
        }
        None => i18n::t("settings-models-no-path"),
    };
    let save_disabled = panel.load_error.is_some() || panel.path.is_none();

    let reload_entity = entity.clone();
    let reload = Button::new("models-reload")
        .label(i18n::t("settings-models-reload"))
        .outline()
        .on_click(move |_ev, window, cx| {
            reload_entity.update(cx, |this, cx| {
                this.models_panel.reload_from_disk(window, cx);
                cx.notify();
            });
        });

    let add_entity = entity.clone();
    let add = Button::new("models-add-provider")
        .label(i18n::t("settings-models-add-provider"))
        .outline()
        .icon(Icon::new(IconName::Plus))
        .on_click(move |_ev, window, cx| {
            add_entity.update(cx, |this, cx| {
                this.models_panel.add_provider(window, cx);
                cx.notify();
            });
        });

    let save = Button::new("models-save")
        .label(i18n::t("settings-models-save"))
        .disabled(save_disabled)
        .on_click(move |_ev, window, cx| {
            entity.update(cx, |this, cx| this.models_panel.save(window, cx));
        });

    v_flex()
        .w_full()
        .gap_2()
        .child(
            h_flex()
                .w_full()
                .items_center()
                .justify_between()
                .gap_2()
                .child(
                    div()
                        .text_base()
                        .font_weight(gpui::FontWeight::BLACK)
                        .child(i18n::t("settings-panel-models")),
                )
                .child(h_flex().gap_2().child(reload).child(add).child(save)),
        )
        .child(muted_text(path_caption, muted))
        .into_any_element()
}

fn render_load_error(theme: &Theme, error: String) -> AnyElement {
    section_card(
        theme,
        vec![
            section_header("settings-models-load-error-title"),
            v_flex()
                .px_3()
                .pb_2()
                .gap_1()
                .child(div().text_sm().text_color(theme.danger).child(error))
                .child(muted_text(
                    i18n::t("settings-models-load-error-hint"),
                    theme.muted_foreground,
                ))
                .into_any_element(),
        ],
    )
}

fn render_empty(theme: &Theme, entity: Entity<SettingsView>) -> AnyElement {
    section_card(
        theme,
        vec![
            v_flex()
                .w_full()
                .items_center()
                .gap_2()
                .px_3()
                .py_6()
                .child(muted_text(
                    i18n::t("settings-models-empty"),
                    theme.muted_foreground,
                ))
                .child(add_button(
                    "models-empty-add",
                    "settings-models-add-provider",
                    entity,
                    Arc::new(|this, window, cx| this.models_panel.add_provider(window, cx)),
                ))
                .into_any_element(),
        ],
    )
}

fn render_provider(
    view: &mut SettingsView,
    theme: &Theme,
    entity: Entity<SettingsView>,
    pid: usize,
    cx: &mut Context<SettingsView>,
) -> AnyElement {
    let panel = &view.models_panel;
    let Some(p) = panel.providers.iter().find(|p| p.id == pid) else {
        return div().into_any_element();
    };
    let muted = theme.muted_foreground;
    let divider = theme.border.opacity(0.6);
    let expanded = panel.expanded.contains(&pid);

    let name_value = p.name.read(cx).value();
    let header_label: SharedString = if name_value.trim().is_empty() {
        i18n::t("settings-models-unnamed")
    } else {
        name_value
    };
    let counts = i18n::t_str(
        "settings-models-count",
        &[
            ("models", &p.models.len().to_string()),
            ("endpoints", &p.endpoints.len().to_string()),
        ],
    );

    let toggle_entity = entity.clone();
    let toggle = h_flex()
        .id(format!("models-p{pid}-toggle"))
        .flex_1()
        .min_w_0()
        .items_center()
        .gap_2()
        .px_3()
        .py_2()
        .rounded(theme.radius)
        .cursor_pointer()
        .hover(|s| s.bg(theme.accent.opacity(0.06)))
        .on_click(move |_ev, _window, cx| {
            toggle_entity.update(cx, |this, cx| {
                this.models_panel.toggle_expanded(pid);
                cx.notify();
            });
        })
        .child(
            Icon::new(if expanded {
                IconName::ChevronDown
            } else {
                IconName::ChevronRight
            })
            .small()
            .text_color(muted),
        )
        .child(
            div()
                .text_sm()
                .font_weight(gpui::FontWeight::MEDIUM)
                .truncate()
                .child(header_label),
        )
        .child(muted_text(counts, muted));

    let header_row = h_flex()
        .w_full()
        .items_center()
        .gap_1()
        .pr_2()
        .child(toggle)
        .child(remove_button(
            format!("models-p{pid}-remove"),
            entity.clone(),
            Arc::new(move |this| this.models_panel.remove_provider(pid)),
        ))
        .into_any_element();

    let mut children: Vec<AnyElement> = vec![header_row];
    if expanded {
        children.push(hairline(divider));
        children.push(field_row(
            theme,
            i18n::t("settings-models-row-name"),
            input_field(&p.name),
        ));
        children.push(field_row(
            theme,
            i18n::t("settings-models-row-apikey"),
            input_field(&p.apikey_source),
        ));
        children.push(hairline(divider));
        children.push(render_endpoints(theme, entity.clone(), p));
        children.push(hairline(divider));
        children.push(render_env_section(theme, entity.clone(), pid, p));
        children.push(hairline(divider));
        children.push(render_models_section(theme, entity.clone(), pid, p));
    }
    plain_card(theme, children)
}

fn render_endpoints(theme: &Theme, entity: Entity<SettingsView>, p: &ProviderForm) -> AnyElement {
    let divider = theme.border.opacity(0.6);
    let pid = p.id;
    let mut rows: Vec<AnyElement> = vec![section_header("settings-models-section-endpoints")];
    for (ix, e) in p.endpoints.iter().enumerate() {
        rows.push(render_endpoint(entity.clone(), pid, e));
        if ix + 1 < p.endpoints.len() {
            rows.push(hairline(divider));
        }
    }
    rows.push(
        h_flex()
            .px_3()
            .py_2()
            .child(add_button(
                format!("models-p{pid}-add-endpoint"),
                "settings-models-add-endpoint",
                entity,
                Arc::new(move |this, window, cx| this.models_panel.add_endpoint(window, cx, pid)),
            ))
            .into_any_element(),
    );
    v_flex().w_full().children(rows).into_any_element()
}

fn render_endpoint(entity: Entity<SettingsView>, pid: usize, e: &EndpointForm) -> AnyElement {
    let eid = e.id;
    let wire_options: Vec<(SharedString, SharedString)> = WIRE_APIS
        .iter()
        .map(|w| (SharedString::from(*w), SharedString::from(*w)))
        .collect();
    let copilot_options: Vec<(SharedString, SharedString)> = vec![
        (
            i18n::t("settings-models-value-unset"),
            SharedString::default(),
        ),
        (SharedString::from("api_key"), SharedString::from("api_key")),
        (
            SharedString::from("bearer_token"),
            SharedString::from("bearer_token"),
        ),
    ];

    let wire_entity = entity.clone();
    let copilot_entity = entity.clone();

    v_flex()
        .w_full()
        .gap_1()
        .px_3()
        .py_2()
        .child(
            h_flex()
                .w_full()
                .items_center()
                .gap_2()
                .child(token_dropdown(
                    format!("models-e{eid}-wire"),
                    e.wire_api.clone(),
                    wire_options,
                    wire_entity,
                    Arc::new(move |this, v| {
                        if let Some(e) = this.models_panel.endpoint_mut(pid, eid) {
                            e.wire_api = v;
                        }
                    }),
                ))
                .child(div().flex_1().min_w_0().child(Input::new(&e.url)))
                .child(remove_button(
                    format!("models-e{eid}-remove"),
                    entity.clone(),
                    Arc::new(move |this| this.models_panel.remove_endpoint(pid, eid)),
                )),
        )
        .child(
            h_flex()
                .w_full()
                .items_center()
                .gap_2()
                .child(div().flex_1().min_w_0().child(Input::new(&e.agents)))
                .child(token_dropdown(
                    format!("models-e{eid}-copilot"),
                    e.copilot_auth.clone(),
                    copilot_options,
                    copilot_entity,
                    Arc::new(move |this, v| {
                        if let Some(e) = this.models_panel.endpoint_mut(pid, eid) {
                            e.copilot_auth = v;
                        }
                    }),
                )),
        )
        .into_any_element()
}

fn render_env_section(
    _theme: &Theme,
    entity: Entity<SettingsView>,
    pid: usize,
    p: &ProviderForm,
) -> AnyElement {
    let mut rows: Vec<AnyElement> = vec![section_header("settings-models-section-env")];
    rows.extend(render_kv_rows(
        entity.clone(),
        format!("models-p{pid}"),
        &p.env,
    ));
    rows.push(
        h_flex()
            .px_3()
            .py_2()
            .child(add_button(
                format!("models-p{pid}-add-env"),
                "settings-models-add-env",
                entity,
                Arc::new(move |this, window, cx| {
                    this.models_panel.add_provider_env(window, cx, pid)
                }),
            ))
            .into_any_element(),
    );
    v_flex().w_full().children(rows).into_any_element()
}

fn render_models_section(
    theme: &Theme,
    entity: Entity<SettingsView>,
    pid: usize,
    p: &ProviderForm,
) -> AnyElement {
    let divider = theme.border.opacity(0.6);
    let mut rows: Vec<AnyElement> = vec![section_header("settings-models-section-models")];

    let inline_active = !p.remote_models;
    rows.push(
        h_flex()
            .px_3()
            .py_1()
            .gap_2()
            .child(mode_button(
                format!("models-p{pid}-mode-inline"),
                inline_active,
                "settings-models-mode-inline",
                entity.clone(),
                Arc::new(move |this| {
                    if let Some(p) = this.models_panel.provider_mut(pid) {
                        p.remote_models = false;
                    }
                }),
            ))
            .child(mode_button(
                format!("models-p{pid}-mode-remote"),
                p.remote_models,
                "settings-models-mode-remote",
                entity.clone(),
                Arc::new(move |this| {
                    if let Some(p) = this.models_panel.provider_mut(pid) {
                        p.remote_models = true;
                    }
                }),
            ))
            .into_any_element(),
    );

    if p.remote_models {
        rows.push(field_row(
            theme,
            i18n::t("settings-models-row-url"),
            input_field(&p.remote_url),
        ));
    } else {
        for (ix, m) in p.models.iter().enumerate() {
            rows.push(render_model(theme, entity.clone(), pid, m));
            if ix + 1 < p.models.len() {
                rows.push(hairline(divider));
            }
        }
        rows.push(
            h_flex()
                .px_3()
                .py_2()
                .child(add_button(
                    format!("models-p{pid}-add-model"),
                    "settings-models-add-model",
                    entity,
                    Arc::new(move |this, window, cx| this.models_panel.add_model(window, cx, pid)),
                ))
                .into_any_element(),
        );
    }
    v_flex().w_full().children(rows).into_any_element()
}

fn render_model(
    theme: &Theme,
    entity: Entity<SettingsView>,
    pid: usize,
    m: &ModelForm,
) -> AnyElement {
    let mid = m.id;
    let muted = theme.muted_foreground;

    let tools_entity = entity.clone();
    let images_entity = entity.clone();

    let mut env_rows: Vec<AnyElement> =
        render_kv_rows(entity.clone(), format!("models-m{mid}"), &m.env);
    env_rows.push(
        h_flex()
            .py_1()
            .child(add_button(
                format!("models-m{mid}-add-env"),
                "settings-models-add-env",
                entity.clone(),
                Arc::new(move |this, window, cx| {
                    this.models_panel.add_model_env(window, cx, pid, mid)
                }),
            ))
            .into_any_element(),
    );

    v_flex()
        .w_full()
        .gap_1()
        .px_3()
        .py_2()
        .child(
            h_flex()
                .w_full()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .w(px(KEY_W))
                        .flex_shrink_0()
                        .child(Input::new(&m.model_id)),
                )
                .child(muted_text(i18n::t("settings-models-row-model-id"), muted))
                .child(h_flex().flex_1())
                .child(remove_button(
                    format!("models-m{mid}-remove"),
                    entity.clone(),
                    Arc::new(move |this| this.models_panel.remove_model(pid, mid)),
                )),
        )
        .child(field_row(
            theme,
            i18n::t("settings-models-row-desc"),
            input_field(&m.desc),
        ))
        .child(
            h_flex()
                .w_full()
                .gap_2()
                .child(div().flex_1().min_w_0().child(field_row(
                    theme,
                    i18n::t("settings-models-row-context"),
                    input_field(&m.context),
                )))
                .child(div().flex_1().min_w_0().child(field_row(
                    theme,
                    i18n::t("settings-models-row-max-tokens"),
                    input_field(&m.max_tokens),
                ))),
        )
        .child(field_row(
            theme,
            i18n::t("settings-models-row-wire-apis"),
            h_flex()
                .gap_4()
                .items_center()
                .child(wire_checkbox(
                    entity.clone(),
                    format!("models-m{mid}-wire-anthropic"),
                    "anthropic",
                    m.wire_anthropic,
                    pid,
                    mid,
                ))
                .child(wire_checkbox(
                    entity.clone(),
                    format!("models-m{mid}-wire-responses"),
                    "responses",
                    m.wire_responses,
                    pid,
                    mid,
                ))
                .child(wire_checkbox(
                    entity.clone(),
                    format!("models-m{mid}-wire-completions"),
                    "completions",
                    m.wire_completions,
                    pid,
                    mid,
                ))
                .into_any_element(),
        ))
        .child(field_row(
            theme,
            i18n::t("settings-models-row-agents"),
            input_field(&m.agents),
        ))
        .child(field_row(
            theme,
            i18n::t("settings-models-row-supports-tools"),
            tri_state_dropdown(
                format!("models-m{mid}-tools"),
                m.supports_tools.clone(),
                tools_entity,
                Arc::new(move |this, v| {
                    if let Some(m) = this.models_panel.model_mut(pid, mid) {
                        m.supports_tools = v;
                    }
                }),
            ),
        ))
        .child(field_row(
            theme,
            i18n::t("settings-models-row-supports-images"),
            tri_state_dropdown(
                format!("models-m{mid}-images"),
                m.supports_images.clone(),
                images_entity,
                Arc::new(move |this, v| {
                    if let Some(m) = this.models_panel.model_mut(pid, mid) {
                        m.supports_images = v;
                    }
                }),
            ),
        ))
        .child(v_flex().w_full().children(env_rows))
        .into_any_element()
}

// --- Element builders -----------------------------------------------------

fn plain_card(theme: &Theme, children: Vec<AnyElement>) -> AnyElement {
    v_flex()
        .w_full()
        .p_2()
        .gap_0()
        .rounded(px(10.))
        .bg(theme.secondary.opacity(0.45))
        .children(children)
        .into_any_element()
}

/// Label-left / control-right row for form fields.
fn field_row(theme: &Theme, label: SharedString, control: AnyElement) -> AnyElement {
    h_flex()
        .w_full()
        .items_center()
        .gap_3()
        .px_3()
        .py_1p5()
        .child(
            div()
                .w(px(LABEL_W))
                .flex_shrink_0()
                .text_sm()
                .text_color(theme.foreground)
                .child(label),
        )
        .child(h_flex().flex_1().min_w_0().child(control))
        .into_any_element()
}

fn input_field(state: &Entity<InputState>) -> AnyElement {
    div().w_full().child(Input::new(state)).into_any_element()
}

/// Dropdown over `(display, token)` pairs; the selected token is applied via
/// `apply` and only the display value flips on the button label.
fn token_dropdown(
    id: impl Into<gpui::ElementId>,
    current: SharedString,
    options: Vec<(SharedString, SharedString)>,
    entity: Entity<SettingsView>,
    apply: TokenApply,
) -> AnyElement {
    let label = options
        .iter()
        .find(|(_, token)| *token == current)
        .map(|(display, _)| display.clone())
        .unwrap_or(current);
    Button::new(id)
        .label(label)
        .dropdown_caret(true)
        .outline()
        .dropdown_menu_with_anchor(Anchor::BottomRight, move |menu, _window, _cx| {
            options.iter().fold(menu, |menu, (display, token)| {
                let display = display.clone();
                let token = token.clone();
                let entity = entity.clone();
                let apply = apply.clone();
                menu.item(
                    PopupMenuItem::new(display.clone()).on_click(move |_ev, _window, cx| {
                        entity.update(cx, |this, cx| {
                            apply(this, token.clone());
                            cx.notify();
                        });
                    }),
                )
            })
        })
        .into_any_element()
}

fn tri_state_dropdown(
    id: impl Into<gpui::ElementId>,
    current: SharedString,
    entity: Entity<SettingsView>,
    apply: TokenApply,
) -> AnyElement {
    let options: Vec<(SharedString, SharedString)> = vec![
        (
            i18n::t("settings-models-value-unset"),
            SharedString::default(),
        ),
        (SharedString::from("true"), SharedString::from("true")),
        (SharedString::from("false"), SharedString::from("false")),
    ];
    token_dropdown(id, current, options, entity, apply)
}

fn wire_checkbox(
    entity: Entity<SettingsView>,
    id: impl Into<gpui::ElementId>,
    wire: &'static str,
    checked: bool,
    pid: usize,
    mid: usize,
) -> AnyElement {
    Checkbox::new(id)
        .label(wire)
        .checked(checked)
        .on_click(move |&new_val, _window, cx| {
            entity.update(cx, |this, cx| {
                if let Some(m) = this.models_panel.model_mut(pid, mid) {
                    match wire {
                        "anthropic" => m.wire_anthropic = new_val,
                        "responses" => m.wire_responses = new_val,
                        "completions" => m.wire_completions = new_val,
                        // Checkboxes are only constructed for `WIRE_APIS`.
                        _ => {}
                    }
                }
                cx.notify();
            });
        })
        .into_any_element()
}

/// Segmented-style mode button; the active one is filled, others outlined.
fn mode_button(
    id: impl Into<gpui::ElementId>,
    active: bool,
    label_key: &'static str,
    entity: Entity<SettingsView>,
    apply: MutApply,
) -> AnyElement {
    let button = Button::new(id).label(i18n::t(label_key)).small();
    let button = if active { button } else { button.outline() };
    button
        .on_click(move |_ev, _window, cx| {
            entity.update(cx, |this, cx| {
                apply(this);
                cx.notify();
            });
        })
        .into_any_element()
}

fn add_button(
    id: impl Into<gpui::ElementId>,
    label_key: &'static str,
    entity: Entity<SettingsView>,
    apply: WindowApply,
) -> AnyElement {
    Button::new(id)
        .label(i18n::t(label_key))
        .small()
        .outline()
        .icon(Icon::new(IconName::Plus))
        .on_click(move |_ev, window, cx| {
            entity.update(cx, |this, cx| {
                apply(this, window, cx);
                cx.notify();
            });
        })
        .into_any_element()
}

fn remove_button(
    id: impl Into<gpui::ElementId>,
    entity: Entity<SettingsView>,
    apply: MutApply,
) -> AnyElement {
    Button::new(id)
        .icon(Icon::new(IconName::Delete))
        .ghost()
        .on_click(move |_ev, _window, cx| {
            entity.update(cx, |this, cx| {
                apply(this);
                cx.notify();
            });
        })
        .into_any_element()
}

/// Key/value input rows shared by provider-level and model-level `env`
/// blocks. Removal dispatches through `remove` with the row's form id.
fn render_kv_rows(
    entity: Entity<SettingsView>,
    id_prefix: String,
    rows: &[KvForm],
) -> Vec<AnyElement> {
    rows.iter()
        .map(|kv| {
            let kid = kv.id;
            let prefix = id_prefix.clone();
            h_flex()
                .w_full()
                .items_center()
                .gap_2()
                .px_3()
                .py_1()
                .child(
                    div()
                        .w(px(KEY_W))
                        .flex_shrink_0()
                        .child(Input::new(&kv.key)),
                )
                .child(div().flex_1().min_w_0().child(Input::new(&kv.value)))
                .child(remove_button(
                    format!("{prefix}-kv{kid}-remove"),
                    entity.clone(),
                    Arc::new(move |this| {
                        // The caller's removal closure captures the scope; row
                        // ids are unique per panel so a flat scan suffices.
                        remove_kv_by_id(&mut this.models_panel, kid);
                    }),
                ))
                .into_any_element()
        })
        .collect()
}

/// Remove a kv row from whichever provider/model env block owns it.
/// Form ids are unique panel-wide, so a single scan is unambiguous.
fn remove_kv_by_id(panel: &mut ModelsPanelState, id: usize) {
    for p in &mut panel.providers {
        if p.env.iter().any(|kv| kv.id == id) {
            p.env.retain(|kv| kv.id != id);
            return;
        }
        for m in &mut p.models {
            if m.env.iter().any(|kv| kv.id == id) {
                m.env.retain(|kv| kv.id != id);
                return;
            }
        }
    }
}
