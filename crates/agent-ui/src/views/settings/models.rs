//! Settings → General → Models panel.
//!
//! Two-column form view over the cx providers config
//! (`~/.config/cx/cx.providers.config.yaml`, schema: `cx_providers::CxConfig`).
//! The left column lists provider cards — each expands inline to edit basic
//! info, provider-level env and wire API endpoints; the right column shows the
//! selected provider's models (LLMs). Opening the panel parses the file into
//! per-field input state (回显); every edit schedules a debounced autosave
//! that validates the form, atomically writes the whole config back —
//! preserving the top-level `agents:` section verbatim — and reloads the
//! process-wide provider registry on a background thread so new threads pick
//! up the change.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    Anchor, AnyElement, AnyWindowHandle, App, AppContext as _, Context, Entity,
    InteractiveElement as _, IntoElement as _, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, div, px,
};
use gpui_component::theme::Theme;
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants},
    checkbox::Checkbox,
    h_flex,
    input::{Input, InputEvent, InputState},
    menu::{DropdownMenu as _, PopupMenuItem},
    notification::Notification,
    tab::{Tab, TabBar},
    v_flex,
};

use agent::i18n;
use cx_providers::{
    AgentConfig, ApiKeySourceKind, CxConfig, ProviderConfig, ProviderEndpointDetail,
    ProviderEndpointSpec, ProviderModelConfig, ProviderModels, active_provider_config_path,
    known_agent_ids, read_config_file, split_apikey_source, write_config_file,
};

use super::SettingsView;
use super::panels::{hairline, muted_text, section_card, section_header};

const LABEL_W: f32 = 150.;
const KEY_W: f32 = 220.;
const LEFT_COL_W: f32 = 480.;
const AUTOSAVE_DEBOUNCE_MS: u64 = 600;
const WIRE_APIS: [&str; 3] = ["anthropic", "responses", "completions"];

/// User-facing wire API names, shared by the endpoint dropdown and the model
/// Wire API checkboxes.
fn wire_display(wire: &str) -> &str {
    match wire {
        "anthropic" => "Anthropic Messages",
        "responses" => "OpenAI Responses",
        "completions" => "OpenAI Completions",
        other => other,
    }
}

fn kind_token(kind: ApiKeySourceKind) -> &'static str {
    match kind {
        ApiKeySourceKind::Env => "env",
        ApiKeySourceKind::Keychain => "keychain",
        ApiKeySourceKind::Shell => "shell",
        ApiKeySourceKind::Literal | ApiKeySourceKind::None => "literal",
    }
}

fn kind_from_token(token: &str) -> ApiKeySourceKind {
    match token {
        "env" => ApiKeySourceKind::Env,
        "keychain" => ApiKeySourceKind::Keychain,
        "shell" => ApiKeySourceKind::Shell,
        _ => ApiKeySourceKind::Literal,
    }
}

fn kind_label(kind: ApiKeySourceKind) -> SharedString {
    match kind {
        ApiKeySourceKind::Env => i18n::t("settings-models-apikey-env"),
        ApiKeySourceKind::Keychain => i18n::t("settings-models-apikey-keychain"),
        ApiKeySourceKind::Shell => i18n::t("settings-models-apikey-shell"),
        ApiKeySourceKind::Literal | ApiKeySourceKind::None => {
            i18n::t("settings-models-apikey-literal")
        }
    }
}

fn kind_placeholder(kind: ApiKeySourceKind) -> &'static str {
    match kind {
        ApiKeySourceKind::Env => "ENV_VAR_NAME",
        ApiKeySourceKind::Keychain => "SERVICE_NAME",
        ApiKeySourceKind::Shell => "command, e.g. op read ...",
        ApiKeySourceKind::Literal | ApiKeySourceKind::None => "sk-...",
    }
}

// --- Panel state ----------------------------------------------------------

/// Everything the Models panel knows about the config file. Built once when
/// the Settings view is created; autosave serializes it back on edit.
pub struct ModelsPanelState {
    /// Resolved config file path (`None` only when `$HOME` is unresolvable).
    path: Option<PathBuf>,
    /// Read/parse failure message. Autosave stays disabled while set so an
    /// unrecognized file is never clobbered.
    load_error: Option<String>,
    /// Top-level `agents:` section — out of scope for this form, preserved
    /// verbatim across saves.
    agents: Vec<AgentConfig>,
    /// Candidate agent ids for the tag pickers (built-ins + configured).
    agent_options: Vec<String>,
    providers: Vec<ProviderForm>,
    /// Provider form id whose models fill the right column.
    selected: Option<usize>,
    /// Provider form ids whose card body is expanded.
    expanded: HashSet<usize>,
    /// Monotonic id source so GPUI element ids stay unique across
    /// add/remove churn.
    next_form_id: usize,
    /// Debounce token for autosave: each `touch` bumps it; a pending save
    /// task only writes when its captured generation is still current.
    save_generation: usize,
    /// Handle to the settings window, kept so the debounced autosave task
    /// (which only sees an `AsyncApp`) can still surface toasts.
    window_handle: AnyWindowHandle,
}

struct ProviderForm {
    id: usize,
    name: Entity<InputState>,
    apikey_kind: ApiKeySourceKind,
    apikey_value: Entity<InputState>,
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
    /// Selected `agents:` override filter; empty = all agents.
    agents: Vec<String>,
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
    /// Selected `agents:` override filter; empty = all agents.
    agents: Vec<String>,
    env: Vec<KvForm>,
    /// Effective value: config null resolves to the documented default
    /// (tools = true, images = false) at load time.
    supports_tools: bool,
    supports_images: bool,
}

type MutApply = Arc<dyn Fn(&mut SettingsView, &mut Context<SettingsView>) + Send + Sync + 'static>;
type WindowApply =
    Arc<dyn Fn(&mut SettingsView, &mut Window, &mut Context<SettingsView>) + Send + Sync + 'static>;
type TokenApply =
    Arc<dyn Fn(&mut SettingsView, SharedString, &mut Context<SettingsView>) + Send + Sync + 'static>;
type TagsApply =
    Arc<dyn Fn(&mut SettingsView, Vec<String>, &mut Context<SettingsView>) + Send + Sync + 'static>;

impl ModelsPanelState {
    /// Read the active config file and build the form state. A missing file
    /// is not an error — saving creates it.
    pub fn load(window: &mut Window, cx: &mut Context<SettingsView>) -> Self {
        let mut state = Self {
            path: None,
            load_error: None,
            agents: Vec::new(),
            agent_options: Vec::new(),
            providers: Vec::new(),
            selected: None,
            expanded: HashSet::new(),
            next_form_id: 1,
            save_generation: 0,
            window_handle: window.window_handle(),
        };
        state.reload_from_disk(window, cx);
        state
    }

    /// Re-read the config file, discarding any unsaved edits.
    pub fn reload_from_disk(&mut self, window: &mut Window, cx: &mut Context<SettingsView>) {
        self.agents.clear();
        self.providers.clear();
        self.expanded.clear();
        self.selected = None;
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
        self.agent_options = known_agent_ids(&config);
        self.agents = config.agents;
        self.providers = config
            .providers
            .into_iter()
            .map(|p| ProviderForm::from_config(p, window, cx, &mut self.next_form_id))
            .collect();
        // Select the first provider so the right column is never idle; only
        // its card starts expanded.
        self.selected = self.providers.first().map(|p| p.id);
        self.expanded = self.selected.into_iter().collect();
        self.load_error = None;
    }

    /// Mark the form dirty and schedule a debounced autosave.
    pub fn touch(&mut self, cx: &mut Context<SettingsView>) {
        if self.load_error.is_some() || self.path.is_none() {
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
            let stale =
                entity.read_with(cx, |this, _| this.models_panel.save_generation != generation);
            if stale {
                return;
            }
            let _ = handle.update(cx, |_view, window, cx| {
                entity.update(cx, |this, cx| {
                    if this.models_panel.save_generation == generation {
                        this.models_panel.save(window, cx);
                    }
                });
            });
        })
        .detach();
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

    /// Select a provider (right column follows, card expands accordion-style);
    /// clicking the already-selected header only toggles the collapse.
    fn select_provider(&mut self, pid: usize) {
        if self.selected == Some(pid) {
            if !self.expanded.remove(&pid) {
                self.expanded.insert(pid);
            }
            return;
        }
        self.selected = Some(pid);
        self.expanded.clear();
        self.expanded.insert(pid);
    }

    fn remove_provider(&mut self, id: usize) {
        self.providers.retain(|p| p.id != id);
        self.expanded.remove(&id);
        if self.selected == Some(id) {
            self.selected = self.providers.first().map(|p| p.id);
            if let Some(sid) = self.selected {
                self.expanded.insert(sid);
            }
        }
    }

    fn add_endpoint(
        &mut self,
        window: &mut Window,
        cx: &mut Context<SettingsView>,
        provider_id: usize,
    ) {
        let id = self.new_form_id();
        if let Some(provider) = self.provider_mut(provider_id) {
            let used: HashSet<&str> = provider
                .endpoints
                .iter()
                .map(|e| e.wire_api.as_str())
                .collect();
            let Some(wire) = WIRE_APIS.iter().find(|w| !used.contains(**w)) else {
                return;
            };
            provider.endpoints.push(EndpointForm {
                id,
                wire_api: SharedString::from(*wire),
                url: new_input(window, cx, "", Some(i18n::t("settings-models-ph-url"))),
                agents: Vec::new(),
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
                context: new_input(window, cx, "", Some(i18n::t("settings-models-ph-context"))),
                max_tokens: new_input(
                    window,
                    cx,
                    "",
                    Some(i18n::t("settings-models-ph-max-tokens")),
                ),
                wire_anthropic: false,
                wire_responses: false,
                wire_completions: false,
                agents: Vec::new(),
                env: Vec::new(),
                supports_tools: true,
                supports_images: false,
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
            let apikey_value = p.apikey_value.read(cx).value();
            let apikey_source = if apikey_value.trim().is_empty() {
                None
            } else {
                p.apikey_kind.build(apikey_value.as_str())
            };

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
                let copilot_auth = optional_text(&e.copilot_auth);
                let spec = if e.agents.is_empty() && copilot_auth.is_none() {
                    ProviderEndpointSpec::Url(url)
                } else {
                    ProviderEndpointSpec::Detailed(ProviderEndpointDetail {
                        url,
                        agents: e.agents.clone(),
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
                            agents: m.agents.clone(),
                            env: collect_env(cx, &m.env, &name, Some(&id))?,
                            max_tokens,
                            context,
                            supports_tools: Some(m.supports_tools),
                            supports_images: Some(m.supports_images),
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
    /// Failures surface as error toasts; success is silent (autosave).
    fn save(&mut self, window: &mut Window, cx: &mut Context<SettingsView>) {
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
        let (apikey_kind, apikey_value) = match config.apikey_source.as_deref() {
            Some(source) => split_apikey_source(source),
            None => (ApiKeySourceKind::Literal, String::new()),
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
                    url: new_input(window, cx, &url, Some(i18n::t("settings-models-ph-url"))),
                    agents,
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
        let remote_models = matches!(config.models, ProviderModels::RemoteUrl(_));
        let remote_url = match &config.models {
            ProviderModels::RemoteUrl(url) => url.clone(),
            ProviderModels::Inline(_) => String::new(),
        };
        let remote_url = new_input(window, cx, &remote_url, Some(i18n::t("settings-models-ph-remote-url")));
        let models = match config.models {
            ProviderModels::Inline(map) => map
                .into_iter()
                .map(|(model_id, m)| ModelForm {
                    id: take_id(),
                    model_id: new_input(window, cx, &model_id, None),
                    desc: new_input(window, cx, m.desc.as_deref().unwrap_or_default(), None),
                    context: new_input(
                        window,
                        cx,
                        &m.context.map(|v| v.to_string()).unwrap_or_default(),
                        Some(i18n::t("settings-models-ph-context")),
                    ),
                    max_tokens: new_input(
                        window,
                        cx,
                        &m.max_tokens.map(|v| v.to_string()).unwrap_or_default(),
                        Some(i18n::t("settings-models-ph-max-tokens")),
                    ),
                    wire_anthropic: m.wire_apis.iter().any(|w| w == "anthropic"),
                    wire_responses: m.wire_apis.iter().any(|w| w == "responses"),
                    wire_completions: m.wire_apis.iter().any(|w| w == "completions"),
                    agents: m.agents,
                    env: m
                        .env
                        .into_iter()
                        .map(|(key, value)| KvForm {
                            id: take_id(),
                            key: new_input(window, cx, &key, None),
                            value: new_input(window, cx, &value, None),
                        })
                        .collect(),
                    supports_tools: m.supports_tools.unwrap_or(true),
                    supports_images: m.supports_images.unwrap_or(false),
                })
                .collect(),
            ProviderModels::RemoteUrl(_) => Vec::new(),
        };
        ProviderForm {
            id: take_id(),
            name: new_input(
                window,
                cx,
                &config.name,
                Some(i18n::t("settings-models-ph-name")),
            ),
            apikey_kind,
            apikey_value: new_input(
                window,
                cx,
                &apikey_value,
                Some(SharedString::from(kind_placeholder(apikey_kind))),
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
    placeholder: Option<SharedString>,
) -> Entity<InputState> {
    let value = SharedString::from(value.to_string());
    let state = cx.new(|cx| {
        let mut state = match placeholder {
            Some(placeholder) => InputState::new(window, cx).placeholder(placeholder),
            None => InputState::new(window, cx),
        };
        if !value.is_empty() {
            state.set_value(value, window, cx);
        }
        state
    });
    // Text edits flow into the same debounced autosave as discrete picks.
    cx.subscribe(&state, |this, _state, event, cx| {
        if matches!(event, InputEvent::Change) {
            this.models_panel.touch(cx);
        }
    })
    .detach();
    state
}

fn optional_text(value: &SharedString) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
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

/// Right-pane renderer for Settings → General → Models: two independently
/// scrolling columns — provider cards left, the selected provider's models
/// right.
pub fn render_models(view: &mut SettingsView, cx: &mut Context<SettingsView>) -> AnyElement {
    let theme = cx.theme().clone();
    let entity = cx.entity();

    let mut left: Vec<AnyElement> = Vec::new();
    if let Some(err) = view.models_panel.load_error.clone() {
        left.push(render_load_error(&theme, err));
    } else if view.models_panel.providers.is_empty() {
        left.push(render_empty(&theme));
    }
    let provider_ids: Vec<usize> = view.models_panel.providers.iter().map(|p| p.id).collect();
    for pid in provider_ids {
        left.push(render_provider(view, &theme, entity.clone(), pid, cx));
    }

    let right = render_models_column(view, &theme, entity.clone(), cx);

    v_flex()
        .w_full()
        .h_full()
        .min_h_0()
        .gap_3()
        .p_4()
        .child(
            div()
                .text_base()
                .font_weight(gpui::FontWeight::BLACK)
                .child(i18n::t("settings-panel-models")),
        )
        .child(
            h_flex()
                .flex_1()
                .min_h_0()
                .w_full()
                .gap_4()
                .child(
                    div()
                        .id("settings-models-left")
                        .w(px(LEFT_COL_W))
                        .flex_shrink_0()
                        .h_full()
                        .min_h_0()
                        .overflow_y_scroll()
                        .pr_1()
                        .child(v_flex().w_full().gap_3().children(left)),
                )
                .child(
                    div()
                        .id("settings-models-right")
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .min_h_0()
                        .overflow_y_scroll()
                        .pr_1()
                        .child(right),
                ),
        )
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

fn render_empty(theme: &Theme) -> AnyElement {
    section_card(
        theme,
        vec![v_flex()
            .w_full()
            .items_center()
            .px_3()
            .py_6()
            .child(muted_text(
                i18n::t("settings-models-empty"),
                theme.muted_foreground,
            ))
            .into_any_element()],
    )
}

/// Left-column provider card: header row plus, when expanded, the 基本信息 /
/// 环境变量 / 端点 modules in order.
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
    let selected = panel.selected == Some(pid);

    let name_value = p.name.read(cx).value();
    let header_label: SharedString = if name_value.trim().is_empty() {
        i18n::t("settings-models-unnamed")
    } else {
        name_value
    };

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
                this.models_panel.select_provider(pid);
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
        );

    let header_row = h_flex()
        .w_full()
        .items_center()
        .gap_2()
        .child(toggle)
        .child(remove_button(
            format!("models-p{pid}-remove"),
            entity.clone(),
            Arc::new(move |this, cx| {
                this.models_panel.remove_provider(pid);
                this.models_panel.touch(cx);
            }),
        ))
        .into_any_element();

    let mut children: Vec<AnyElement> = vec![header_row];
    if expanded {
        children.push(hairline(divider));
        children.push(render_basic_section(theme, entity.clone(), pid, p));
        children.push(hairline(divider));
        children.push(render_env_section(theme, entity.clone(), pid, p));
        children.push(hairline(divider));
        let agent_options = view.models_panel.agent_options.clone();
        children.push(render_endpoints(theme, entity.clone(), pid, p, &agent_options));
    }
    provider_card(theme, selected, children)
}

/// Rounded card for a provider; the selected one gets an accent border so the
/// left/right column linkage reads at a glance.
fn provider_card(theme: &Theme, selected: bool, children: Vec<AnyElement>) -> AnyElement {
    let border = if selected {
        theme.accent.opacity(0.55)
    } else {
        theme.border.opacity(0.35)
    };
    v_flex()
        .w_full()
        .p_2()
        .gap_0()
        .rounded(px(10.))
        .bg(theme.secondary.opacity(0.45))
        .border_1()
        .border_color(border)
        .children(children)
        .into_any_element()
}

/// 基本信息 module: name plus the API Key kind dropdown + value input pair.
fn render_basic_section(
    theme: &Theme,
    entity: Entity<SettingsView>,
    pid: usize,
    p: &ProviderForm,
) -> AnyElement {
    let kind_options: Vec<(SharedString, SharedString)> = [
        ApiKeySourceKind::Literal,
        ApiKeySourceKind::Env,
        ApiKeySourceKind::Keychain,
        ApiKeySourceKind::Shell,
    ]
    .iter()
    .map(|k| (kind_label(*k), SharedString::from(kind_token(*k))))
    .collect();

    let kind_entity = entity.clone();
    let apikey_control = h_flex()
        .w_full()
        .gap_2()
        .child(token_dropdown(
            format!("models-p{pid}-apikey-kind"),
            SharedString::from(kind_token(p.apikey_kind)),
            kind_options,
            kind_entity,
            Arc::new(move |this, v, cx| {
                if let Some(p) = this.models_panel.provider_mut(pid) {
                    p.apikey_kind = kind_from_token(&v);
                    this.models_panel.touch(cx);
                }
            }),
        ))
        .child(div().flex_1().min_w_0().child(Input::new(&p.apikey_value)))
        .into_any_element();

    v_flex()
        .w_full()
        .children(vec![
            section_header("settings-models-section-basic"),
            field_row(
                theme,
                i18n::t("settings-models-row-name"),
                input_field(&p.name),
            ),
            field_row(theme, i18n::t("settings-models-row-apikey"), apikey_control),
        ])
        .into_any_element()
}

/// 环境变量 module: add button first, then key/value rows.
fn render_env_section(
    _theme: &Theme,
    entity: Entity<SettingsView>,
    pid: usize,
    p: &ProviderForm,
) -> AnyElement {
    let mut rows: Vec<AnyElement> = vec![section_header("settings-models-section-env")];
    rows.push(
        h_flex()
            .px_3()
            .py_1()
            .child(add_button(
                format!("models-p{pid}-add-env"),
                "settings-models-add-env",
                entity.clone(),
                Arc::new(move |this, window, cx| {
                    this.models_panel.add_provider_env(window, cx, pid);
                    this.models_panel.touch(cx);
                }),
            ))
            .into_any_element(),
    );
    rows.extend(render_kv_rows(entity.clone(), format!("models-p{pid}"), &p.env));
    v_flex().w_full().children(rows).into_any_element()
}

/// 端点 module: add button first (hidden once all three wire APIs are taken),
/// then one row per endpoint.
fn render_endpoints(
    theme: &Theme,
    entity: Entity<SettingsView>,
    pid: usize,
    p: &ProviderForm,
    agent_options: &[String],
) -> AnyElement {
    let divider = theme.border.opacity(0.6);
    let used: HashSet<String> = p
        .endpoints
        .iter()
        .map(|e| e.wire_api.to_string())
        .collect();

    let mut rows: Vec<AnyElement> = vec![section_header("settings-models-section-endpoints")];
    if used.len() < WIRE_APIS.len() {
        rows.push(
            h_flex()
                .px_3()
                .py_1()
                .child(add_button(
                    format!("models-p{pid}-add-endpoint"),
                    "settings-models-add-endpoint",
                    entity.clone(),
                    Arc::new(move |this, window, cx| {
                        this.models_panel.add_endpoint(window, cx, pid);
                        this.models_panel.touch(cx);
                    }),
                ))
                .into_any_element(),
        );
    }
    for (ix, e) in p.endpoints.iter().enumerate() {
        rows.push(render_endpoint(entity.clone(), pid, &used, agent_options, e));
        if ix + 1 < p.endpoints.len() {
            rows.push(hairline(divider));
        }
    }
    v_flex().w_full().children(rows).into_any_element()
}

fn render_endpoint(
    entity: Entity<SettingsView>,
    pid: usize,
    used: &HashSet<String>,
    agent_options: &[String],
    e: &EndpointForm,
) -> AnyElement {
    let eid = e.id;
    let current = e.wire_api.to_string();
    // Each wire API may appear at most once per provider: a row offers its own
    // kind plus the kinds no other row has taken.
    let wire_options: Vec<(SharedString, SharedString)> = WIRE_APIS
        .iter()
        .filter(|w| **w == current || !used.contains(**w))
        .map(|w| (SharedString::from(wire_display(w)), SharedString::from(*w)))
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
    let tags_entity = entity.clone();
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
                    Arc::new(move |this, v, cx| {
                        if let Some(e) = this.models_panel.endpoint_mut(pid, eid) {
                            e.wire_api = v;
                            this.models_panel.touch(cx);
                        }
                    }),
                ))
                .child(div().flex_1().min_w_0().child(Input::new(&e.url)))
                .child(remove_button(
                    format!("models-e{eid}-remove"),
                    entity.clone(),
                    Arc::new(move |this, cx| {
                        this.models_panel.remove_endpoint(pid, eid);
                        this.models_panel.touch(cx);
                    }),
                )),
        )
        .child(
            h_flex()
                .w_full()
                .items_center()
                .gap_2()
                .child(div().flex_1().min_w_0().child(agent_tags(
                    format!("models-e{eid}-agents"),
                    agent_options,
                    &e.agents,
                    tags_entity,
                    Arc::new(move |this, v, cx| {
                        if let Some(e) = this.models_panel.endpoint_mut(pid, eid) {
                            e.agents = v;
                            this.models_panel.touch(cx);
                        }
                    }),
                )))
                .child(token_dropdown(
                    format!("models-e{eid}-copilot"),
                    e.copilot_auth.clone(),
                    copilot_options,
                    copilot_entity,
                    Arc::new(move |this, v, cx| {
                        if let Some(e) = this.models_panel.endpoint_mut(pid, eid) {
                            e.copilot_auth = v;
                            this.models_panel.touch(cx);
                        }
                    }),
                )),
        )
        .into_any_element()
}

/// Right column: the selected provider's models (LLMs), headed by the 模型
/// module title and the 手动配置 / 自动获取 tab.
fn render_models_column(
    view: &mut SettingsView,
    theme: &Theme,
    entity: Entity<SettingsView>,
    _cx: &mut Context<SettingsView>,
) -> AnyElement {
    let muted = theme.muted_foreground;
    let hint = || {
        v_flex()
            .w_full()
            .child(muted_text(i18n::t("settings-models-no-selection"), muted))
            .into_any_element()
    };
    let Some(pid) = view.models_panel.selected else {
        return hint();
    };
    let Some(p) = view.models_panel.providers.iter().find(|p| p.id == pid) else {
        return hint();
    };
    let divider = theme.border.opacity(0.6);
    let agent_options = view.models_panel.agent_options.clone();

    let mut children: Vec<AnyElement> = vec![section_header("settings-models-section-models")];
    children.push(render_mode_tabs(entity.clone(), pid, p));
    if p.remote_models {
        children.push(hairline(divider));
        children.push(field_row(
            theme,
            i18n::t("settings-models-row-url"),
            input_field(&p.remote_url),
        ));
    } else {
        children.push(
            h_flex()
                .px_3()
                .py_1()
                .child(add_button(
                    format!("models-p{pid}-add-model"),
                    "settings-models-add-model",
                    entity.clone(),
                    Arc::new(move |this, window, cx| {
                        this.models_panel.add_model(window, cx, pid);
                        this.models_panel.touch(cx);
                    }),
                ))
                .into_any_element(),
        );
        for (ix, m) in p.models.iter().enumerate() {
            children.push(render_model(theme, entity.clone(), pid, m, &agent_options));
            if ix + 1 < p.models.len() {
                children.push(hairline(divider));
            }
        }
    }
    v_flex()
        .w_full()
        .child(plain_card(theme, children))
        .into_any_element()
}

/// Two-choice tab replacing the old 内联定义 / 远程 URL button pair.
fn render_mode_tabs(entity: Entity<SettingsView>, pid: usize, p: &ProviderForm) -> AnyElement {
    let tab_entity = entity.clone();
    h_flex()
        .px_3()
        .py_1()
        .child(
            TabBar::new(format!("models-p{pid}-mode"))
                .selected_index(if p.remote_models { 1 } else { 0 })
                .on_click(move |ix, _window, cx| {
                    let remote = *ix == 1;
                    tab_entity.update(cx, |this, cx| {
                        if let Some(p) = this
                            .models_panel
                            .provider_mut(pid)
                            .filter(|p| p.remote_models != remote)
                        {
                            p.remote_models = remote;
                            this.models_panel.touch(cx);
                        }
                        cx.notify();
                    });
                })
                .child(Tab::new().label(i18n::t("settings-models-mode-inline")))
                .child(Tab::new().label(i18n::t("settings-models-mode-remote"))),
        )
        .into_any_element()
}

fn render_model(
    theme: &Theme,
    entity: Entity<SettingsView>,
    pid: usize,
    m: &ModelForm,
    agent_options: &[String],
) -> AnyElement {
    let mid = m.id;

    let tools_entity = entity.clone();
    let images_entity = entity.clone();
    let agents_entity = entity.clone();

    // Model-level env block: add button first, then key/value rows.
    let mut env_rows: Vec<AnyElement> = vec![h_flex()
        .py_1()
        .child(add_button(
            format!("models-m{mid}-add-env"),
            "settings-models-add-env",
            entity.clone(),
            Arc::new(move |this, window, cx| {
                this.models_panel.add_model_env(window, cx, pid, mid);
                this.models_panel.touch(cx);
            }),
        ))
        .into_any_element()];
    env_rows.extend(render_kv_rows(entity.clone(), format!("models-m{mid}"), &m.env));

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
                        .w(px(LABEL_W))
                        .flex_shrink_0()
                        .text_sm()
                        .text_color(theme.foreground)
                        .child(i18n::t("settings-models-row-model-id")),
                )
                .child(div().flex_1().min_w_0().child(Input::new(&m.model_id)))
                .child(remove_button(
                    format!("models-m{mid}-remove"),
                    entity.clone(),
                    Arc::new(move |this, cx| {
                        this.models_panel.remove_model(pid, mid);
                        this.models_panel.touch(cx);
                    }),
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
            agent_tags(
                format!("models-m{mid}-agents"),
                agent_options,
                &m.agents,
                agents_entity,
                Arc::new(move |this, v, cx| {
                    if let Some(m) = this.models_panel.model_mut(pid, mid) {
                        m.agents = v;
                        this.models_panel.touch(cx);
                    }
                }),
            ),
        ))
        .child(field_row(
            theme,
            i18n::t("settings-models-row-supports-tools"),
            bool_dropdown(
                format!("models-m{mid}-tools"),
                m.supports_tools,
                tools_entity,
                Arc::new(move |this, v, cx| {
                    if let Some(m) = this.models_panel.model_mut(pid, mid) {
                        m.supports_tools = v == "true";
                        this.models_panel.touch(cx);
                    }
                }),
            ),
        ))
        .child(field_row(
            theme,
            i18n::t("settings-models-row-supports-images"),
            bool_dropdown(
                format!("models-m{mid}-images"),
                m.supports_images,
                images_entity,
                Arc::new(move |this, v, cx| {
                    if let Some(m) = this.models_panel.model_mut(pid, mid) {
                        m.supports_images = v == "true";
                        this.models_panel.touch(cx);
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
                let token = token.clone();
                let entity = entity.clone();
                let apply = apply.clone();
                menu.item(PopupMenuItem::new(display.clone()).on_click(move |_ev, _window, cx| {
                    entity.update(cx, |this, cx| {
                        apply(this, token.clone(), cx);
                        cx.notify();
                    });
                }))
            })
        })
        .into_any_element()
}

/// true/false dropdown for the effective supports_tools / supports_images
/// value (unset config resolves to the documented default at load time).
fn bool_dropdown(
    id: impl Into<gpui::ElementId>,
    current: bool,
    entity: Entity<SettingsView>,
    apply: TokenApply,
) -> AnyElement {
    let options: Vec<(SharedString, SharedString)> = vec![
        (SharedString::from("true"), SharedString::from("true")),
        (SharedString::from("false"), SharedString::from("false")),
    ];
    token_dropdown(
        id,
        SharedString::from(if current { "true" } else { "false" }),
        options,
        entity,
        apply,
    )
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
        .label(wire_display(wire))
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
                    this.models_panel.touch(cx);
                }
                cx.notify();
            });
        })
        .into_any_element()
}

/// Clickable tag picker for `agents:` override filters. Empty selection means
/// all agents; ids already present in the config but missing from the option
/// list are appended so they stay visible and removable.
fn agent_tags(
    id_prefix: String,
    options: &[String],
    selected: &[String],
    entity: Entity<SettingsView>,
    apply: TagsApply,
) -> AnyElement {
    let mut all: Vec<String> = options.to_vec();
    for tag in selected {
        if !all.contains(tag) {
            all.push(tag.clone());
        }
    }
    let chips: Vec<AnyElement> = all
        .iter()
        .map(|tag| {
            let is_on = selected.contains(tag);
            let tag_clone = tag.clone();
            let selected_vec = selected.to_vec();
            let entity = entity.clone();
            let apply = apply.clone();
            let button = Button::new(format!("{id_prefix}-{tag}"))
                .label(SharedString::from(tag.clone()))
                .small();
            let button = if is_on { button } else { button.outline() };
            button
                .on_click(move |_ev, _window, cx| {
                    let mut next = selected_vec.clone();
                    if let Some(pos) = next.iter().position(|t| t == &tag_clone) {
                        next.remove(pos);
                    } else {
                        next.push(tag_clone.clone());
                    }
                    entity.update(cx, |this, cx| {
                        apply(this, next.clone(), cx);
                        cx.notify();
                    });
                })
                .into_any_element()
        })
        .collect();
    let mut col = v_flex()
        .w_full()
        .gap_1()
        .child(h_flex().flex_wrap().gap_1().children(chips));
    if selected.is_empty() {
        col = col.child(muted_text(
            i18n::t("settings-models-agents-all-hint"),
            gpui::transparent_black(),
        ));
    }
    col.into_any_element()
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
                apply(this, cx);
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
                    Arc::new(move |this, cx| {
                        // The caller's removal closure captures the scope; row
                        // ids are unique per panel so a flat scan suffices.
                        remove_kv_by_id(&mut this.models_panel, kid);
                        this.models_panel.touch(cx);
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
