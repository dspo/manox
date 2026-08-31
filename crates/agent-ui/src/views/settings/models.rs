//! Settings → General → Models panel.
//!
//! Two-column form view over the cx providers config
//! (`~/.manox/cx.providers.config.yaml`, schema: `cx_providers::CxConfig`).
//! The left column lists provider cards (accordion, double-click header to
//! rename); the expanded card renders whichever module the right-hand module
//! nav has selected — 基本信息 / 环境变量 / 端点信息 / 模型列表. Every edit
//! schedules a debounced autosave that validates the form, atomically writes
//! the whole config back — preserving the top-level `agents:` section
//! verbatim — and reloads the process-wide provider registry on a background
//! thread so new threads pick up the change.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    Anchor, AnyElement, AnyWindowHandle, App, AppContext as _, Context, Entity, Focusable,
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
    v_flex,
};

use agent::i18n;
use cx_providers::{
    AgentConfig, ApiKeySourceKind, CxConfig, ProviderConfig, ProviderEndpointDetail,
    ProviderEndpointSpec, ProviderModelConfig, ProviderModels, active_provider_config_path,
    agent_display_name, canonical_agent_id, known_agent_ids, normalize_agent_configs,
    normalize_agent_ids, read_config_file, split_apikey_source, write_config_file,
};

use super::SettingsView;
use super::panels::{
    blur_on_click_out, defer_blur_flush, muted_text, section_card, section_header,
};

const LABEL_W: f32 = 150.;
const KEY_W: f32 = 220.;
const NAV_COL_W: f32 = 230.;
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

/// The copilot auth row applies only when the endpoint's agents allow-list
/// covers GitHub Copilot (an empty list means all agents).
fn agents_include_copilot(agents: &[String]) -> bool {
    agents.is_empty() || agents.iter().any(|a| canonical_agent_id(a) == "copilot")
}

/// The four modules the right-hand nav offers; the expanded provider card on
/// the left renders exactly one of them at a time.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ModuleTab {
    Basic,
    Env,
    Endpoints,
    Models,
}

const MODULE_TABS: [ModuleTab; 4] = [
    ModuleTab::Basic,
    ModuleTab::Env,
    ModuleTab::Endpoints,
    ModuleTab::Models,
];

fn module_label(module: ModuleTab) -> SharedString {
    match module {
        ModuleTab::Basic => i18n::t("settings-models-section-basic"),
        ModuleTab::Env => i18n::t("settings-models-section-env"),
        ModuleTab::Endpoints => i18n::t("settings-models-section-endpoints"),
        ModuleTab::Models => i18n::t("settings-models-section-models"),
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
    /// Provider form id whose card is expanded and whose data fills the form.
    selected: Option<usize>,
    /// Provider form ids whose card body is expanded.
    expanded: HashSet<usize>,
    /// Provider form ids whose header is in double-click rename mode.
    renaming: HashSet<usize>,
    /// Delete button armed for two-step confirmation (element id key).
    pending_delete: Option<String>,
    /// Module currently rendered in the expanded provider card.
    module: ModuleTab,
    /// Monotonic id source so GPUI element ids stay unique across
    /// add/remove churn.
    next_form_id: usize,
    /// Debounce token for autosave: each `touch` bumps it; a pending save
    /// task only writes when its captured generation is still current.
    save_generation: usize,
    /// Handle to the settings window, kept so the debounced autosave task
    /// (which only sees an `AsyncApp`) can still surface toasts.
    window_handle: AnyWindowHandle,
    /// Unsaved edits exist (set by `touch`, cleared by a successful `save`).
    dirty: bool,
    /// A save completed since the last blur; the next blur surfaces a "Saved"
    /// toast.
    pending_saved_toast: bool,
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
type TokenApply = Arc<
    dyn Fn(&mut SettingsView, SharedString, &mut Window, &mut Context<SettingsView>)
        + Send
        + Sync
        + 'static,
>;
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
            renaming: HashSet::new(),
            pending_delete: None,
            module: ModuleTab::Basic,
            next_form_id: 1,
            save_generation: 0,
            window_handle: window.window_handle(),
            dirty: false,
            pending_saved_toast: false,
        };
        state.reload_from_disk(window, cx);
        state
    }

    /// Re-read the config file, discarding any unsaved edits.
    pub fn reload_from_disk(&mut self, window: &mut Window, cx: &mut Context<SettingsView>) {
        self.agents.clear();
        self.providers.clear();
        self.expanded.clear();
        self.renaming.clear();
        self.pending_delete = None;
        self.selected = None;
        self.load_error = None;
        // A reload discards unsaved edits; the flush-on-blur flags must not
        // outlive them.
        self.dirty = false;
        self.pending_saved_toast = false;

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
        // Canonicalize + dedupe the preserved `agents:` section so removed
        // legacy ids (codex+) never round-trip back into the config on save.
        self.agents = normalize_agent_configs(config.agents);
        self.providers = config
            .providers
            .into_iter()
            .map(|p| ProviderForm::from_config(p, window, cx, &mut self.next_form_id))
            .collect();
        // Select the first provider so the form is never idle; only its card
        // starts expanded.
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
        self.dirty = true;
        let generation = self.save_generation;
        let entity = cx.entity().clone();
        let handle = self.window_handle;
        cx.spawn(async move |_this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(AUTOSAVE_DEBOUNCE_MS))
                .await;
            let stale = entity.read_with(cx, |this, _| {
                this.models_panel.save_generation != generation
            });
            if stale {
                return;
            }
            let _ = handle.update(cx, |_view, window, cx| {
                entity.update(cx, |this, cx| {
                    if this.models_panel.save_generation == generation {
                        // Debounced safety net stays silent; blur surfaces the
                        // "Saved" toast via `flush_on_blur`.
                        let _ = this.models_panel.save(window, cx);
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

    /// Select a provider (form follows, card expands accordion-style);
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

    /// Double-click rename: put the header into edit mode and focus the name
    /// input. Blur (subscribed at input creation) leaves rename mode.
    fn begin_rename(&mut self, pid: usize, window: &mut Window, cx: &mut Context<SettingsView>) {
        self.renaming.insert(pid);
        if let Some(name) = self.provider_mut(pid).map(|p| p.name.clone()) {
            name.update(cx, |state, cx| state.focus(window, cx));
        }
    }

    fn add_provider(&mut self, window: &mut Window, cx: &mut Context<SettingsView>) {
        let form = ProviderForm::from_config(
            ProviderConfig {
                name: String::new(),
                apikey_source: None,
                models: ProviderModels::Inline(BTreeMap::new()),
                endpoints: BTreeMap::new(),
                env: BTreeMap::new(),
            },
            window,
            cx,
            &mut self.next_form_id,
        );
        let id = form.id;
        self.providers.push(form);
        self.selected = Some(id);
        self.expanded.clear();
        self.expanded.insert(id);
        // Jump straight into rename so the new provider gets a name.
        self.begin_rename(id, window, cx);
    }

    fn remove_provider(&mut self, id: usize) {
        self.providers.retain(|p| p.id != id);
        self.expanded.remove(&id);
        self.renaming.remove(&id);
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

    fn env_rows_mut(&mut self, pid: usize, mid: Option<usize>) -> Option<&mut Vec<KvForm>> {
        let provider = self.provider_mut(pid)?;
        match mid {
            Some(mid) => Some(&mut provider.models.iter_mut().find(|m| m.id == mid)?.env),
            None => Some(&mut provider.env),
        }
    }

    /// Insert an empty env row after `index` (or at the end when `None`) and
    /// focus its key input, ready for typing.
    fn add_env_row(
        &mut self,
        window: &mut Window,
        cx: &mut Context<SettingsView>,
        pid: usize,
        mid: Option<usize>,
        index: Option<usize>,
    ) {
        let id = self.new_form_id();
        if let Some(rows) = self.env_rows_mut(pid, mid) {
            let at = match index {
                Some(ix) => (ix + 1).min(rows.len()),
                None => rows.len(),
            };
            rows.insert(at, KvForm::new(id, window, cx));
            if let Some(key) = rows.get(at).map(|row| row.key.clone()) {
                key.update(cx, |state, cx| state.focus(window, cx));
            }
        }
    }

    fn remove_env_row(&mut self, pid: usize, mid: Option<usize>, id: usize) {
        if let Some(rows) = self.env_rows_mut(pid, mid) {
            rows.retain(|kv| kv.id != id);
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
                let copilot_auth = if agents_include_copilot(&e.agents) {
                    optional_text(&e.copilot_auth)
                } else {
                    None
                };
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
            // Carried over from a fresh disk read in `save()`; this form does
            // not model these sections (External Tools panels + subagent
            // model pinning).
            chatgpt_app: None,
            vscode_app: None,
            subagents: Default::default(),
        })
    }

    /// Validate, write the config file, and refresh the runtime registry.
    /// Failures surface as error toasts and return `false`; a successful
    /// write returns `true` with the runtime reload spawned asynchronously.
    fn save(&mut self, window: &mut Window, cx: &mut Context<SettingsView>) -> bool {
        if self.load_error.is_some() {
            return false;
        }
        let Some(path) = self.path.clone() else {
            window.push_notification(
                Notification::error(i18n::t("settings-models-no-path"))
                    .title(i18n::t("settings-save-failed-title")),
                cx,
            );
            return false;
        };
        let mut config = match self.collect(cx) {
            Ok(config) => config,
            Err(msg) => {
                window.push_notification(
                    Notification::error(msg).title(i18n::t("settings-save-failed-title")),
                    cx,
                );
                return false;
            }
        };
        // The `chatgpt_app:` / `vscode_app:` / `subagents:` sections are not
        // modeled by this form: the first two belong to the External Tools
        // panels, `subagents:` to the host's subagent dispatch — any of them
        // may change on disk after this panel took its open-time snapshot.
        // Carry them over from a fresh disk read so this form never clobbers
        // them. A failed re-read must abort the save (not silently drop the
        // sections): writing without them would erase the panels' settings
        // and the user's subagent model pinning, the exact clobber this
        // carry-over exists to prevent. Mirror the panels' `?` propagation
        // with a toast.
        match read_config_file(&path) {
            Ok(fresh) => carry_over_unmodeled_sections(&mut config, &fresh),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "re-read of provider config failed; aborting Models save to preserve unmodeled sections"
                );
                window.push_notification(
                    Notification::error(e.to_string()).title(i18n::t("settings-save-failed-title")),
                    cx,
                );
                return false;
            }
        }
        if let Err(e) = write_config_file(&path, &config) {
            tracing::warn!(error = %e, "failed to write cx providers config");
            window.push_notification(
                Notification::error(e.to_string()).title(i18n::t("settings-save-failed-title")),
                cx,
            );
            return false;
        }

        self.dirty = false;
        self.pending_saved_toast = true;

        // Rebuild the process-wide registry from the freshly written file so
        // new threads see the change without a restart. Api key resolution
        // may hit the OS keychain or run shell commands, so it runs off the
        // main thread; on failure the previous registry stays live.
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { agent::pi_providers::reload() })
                .await;
            let _ = this.update_in(cx, |_this, window, cx| match result {
                Ok(()) => crate::menu::rebuild_menus(cx),
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

        true
    }

    /// A blur with unsaved edits flushes the pending autosave immediately, then
    /// surfaces the "Saved" toast exactly once per edited field.
    fn flush_on_blur(&mut self, window: &mut Window, cx: &mut Context<SettingsView>) {
        if self.load_error.is_some() || self.path.is_none() {
            return;
        }
        if self.dirty {
            // Invalidate the in-flight debounced save; save now instead.
            self.save_generation = self.save_generation.wrapping_add(1);
            if !self.save(window, cx) {
                return; // `save` already surfaced the error toast.
            }
            self.pending_saved_toast = false;
            window.push_notification(Notification::success(i18n::t("settings-saved")), cx);
        } else if self.pending_saved_toast {
            self.pending_saved_toast = false;
            window.push_notification(Notification::success(i18n::t("settings-saved")), cx);
        }
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
        let name = new_input(
            window,
            cx,
            &config.name,
            Some(i18n::t("settings-models-ph-name")),
        );
        let endpoints = config
            .endpoints
            .into_iter()
            .map(|(wire_api, spec)| {
                let (url, agents, copilot_auth) = match spec {
                    ProviderEndpointSpec::Url(url) => (url, Vec::new(), None),
                    // Nested allow-lists get the same legacy cleanup as the
                    // top-level `agents:` section.
                    ProviderEndpointSpec::Detailed(detail) => (
                        detail.url,
                        normalize_agent_ids(&detail.agents),
                        detail.copilot_auth,
                    ),
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
        let remote_url = new_input(
            window,
            cx,
            &remote_url,
            Some(i18n::t("settings-models-ph-remote-url")),
        );
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
                    agents: normalize_agent_ids(&m.agents),
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
        let id = take_id();
        // Blur on the name input leaves double-click rename mode.
        cx.subscribe(&name, move |this, _state, event, cx| {
            if matches!(event, InputEvent::Blur | InputEvent::PressEnter { .. }) {
                this.models_panel.renaming.remove(&id);
                cx.notify();
            }
        })
        .detach();
        ProviderForm {
            id,
            name,
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
    // Text edits flow into the same debounced autosave as discrete picks; a
    // blur flushes pending edits and surfaces the "Saved" toast.
    cx.subscribe(&state, |this, _state, event, cx| match event {
        InputEvent::Change => this.models_panel.touch(cx),
        InputEvent::Blur => {
            let handle = this.models_panel.window_handle;
            defer_blur_flush(cx, handle, |this, window, cx| {
                this.models_panel.flush_on_blur(window, cx);
            });
        }
        _ => {}
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

/// Right-pane renderer for Settings → General → Models: left column holds the
/// provider accordion (expanded card renders the selected module form); the
/// narrow right column is a module-name nav scoped to the selected provider.
pub fn render_models(view: &mut SettingsView, cx: &mut Context<SettingsView>) -> AnyElement {
    let theme = cx.theme().clone();
    let entity = cx.entity();

    // Left column: provider tree (provider nodes with module children) plus
    // the dashed add-provider button at the list end.
    let mut left: Vec<AnyElement> = Vec::new();
    let provider_ids: Vec<usize> = view.models_panel.providers.iter().map(|p| p.id).collect();
    for pid in provider_ids {
        left.push(render_provider_node(view, &theme, entity.clone(), pid, cx));
    }
    left.push(dashed_add_button(
        &theme,
        "models-add-provider",
        "settings-models-add-provider",
        entity.clone(),
        Arc::new(move |this, window, cx| {
            this.models_panel.add_provider(window, cx);
            this.models_panel.touch(cx);
        }),
    ));

    let right = if let Some(err) = view.models_panel.load_error.clone() {
        render_load_error(&theme, err)
    } else if view.models_panel.providers.is_empty() {
        render_empty(&theme)
    } else {
        render_module_form(view, &theme, entity.clone(), cx)
    };

    v_flex()
        .w_full()
        .h_full()
        .min_h_0()
        .gap_3()
        .p_4()
        .child(
            h_flex()
                .flex_1()
                .min_h_0()
                .w_full()
                .gap_4()
                .child(
                    div()
                        .id("settings-models-left")
                        .w(px(NAV_COL_W))
                        .flex_shrink_0()
                        .h_full()
                        .min_h_0()
                        .overflow_y_scroll()
                        .pr_1()
                        .child(v_flex().w_full().gap_1().children(left)),
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
        vec![
            v_flex()
                .w_full()
                .items_center()
                .px_3()
                .py_6()
                .child(muted_text(
                    i18n::t("settings-models-empty"),
                    theme.muted_foreground,
                ))
                .into_any_element(),
        ],
    )
}

/// Right column: the form of the module selected in the left tree, wrapped in
/// a bordered panel.
fn render_module_form(
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
    let panel = &view.models_panel;
    let Some(pid) = panel.selected else {
        return hint();
    };
    let Some(p) = panel.providers.iter().find(|p| p.id == pid) else {
        return hint();
    };
    let agent_options = panel.agent_options.clone();
    let pending = panel.pending_delete.clone();
    let module = panel.module;
    let body = match module {
        ModuleTab::Basic => render_basic_module(theme, entity.clone(), pid, p),
        ModuleTab::Env => render_env_block(theme, entity.clone(), pid, None, &p.env, &pending),
        ModuleTab::Endpoints => {
            render_endpoints_module(theme, entity.clone(), pid, p, &agent_options, &pending)
        }
        ModuleTab::Models => {
            render_models_module(theme, entity.clone(), pid, p, &agent_options, &pending)
        }
    };
    div()
        .w_full()
        .rounded(px(10.))
        .border_1()
        .border_color(theme.border.opacity(0.6))
        .p_3()
        .child(body)
        .into_any_element()
}

/// Left-column tree node: provider header (double-click renames inline) with
/// the four module names as indented children when expanded; clicking a
/// module child selects (provider, module) for the right-column form.
fn render_provider_node(
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
    let expanded = panel.expanded.contains(&pid);
    let selected = panel.selected == Some(pid);
    let renaming = panel.renaming.contains(&pid);

    let toggle_entity = entity.clone();
    let rename_entity = entity.clone();
    let toggle = h_flex()
        .id(format!("models-p{pid}-toggle"))
        .flex_1()
        .min_w_0()
        .items_center()
        .gap_2()
        .px_2()
        .py_1()
        .rounded(theme.radius)
        .cursor_pointer()
        .hover(|s| s.bg(theme.accent.opacity(0.06)))
        .on_click(move |ev, window, cx| {
            if ev.click_count() >= 2 {
                rename_entity.update(cx, |this, cx| {
                    this.models_panel.begin_rename(pid, window, cx);
                    cx.notify();
                });
            } else {
                toggle_entity.update(cx, |this, cx| {
                    this.models_panel.select_provider(pid);
                    cx.notify();
                });
            }
        })
        .child(
            Icon::new(if expanded {
                IconName::ChevronDown
            } else {
                IconName::ChevronRight
            })
            .small()
            .text_color(muted),
        );
    let name = p.name.clone();
    let out_entity = entity.clone();
    let toggle = if renaming {
        toggle.child(
            div()
                .id(format!("models-p{pid}-rename"))
                .flex_1()
                .min_w_0()
                .on_mouse_down_out(move |_ev, window, cx| {
                    out_entity.update(cx, |this, cx| {
                        this.models_panel.renaming.remove(&pid);
                        cx.notify();
                    });
                    // Exiting rename leaves the caret in the name input;
                    // drop focus so the blur handler surfaces pending edits.
                    let handle = Focusable::focus_handle(&name, cx);
                    if handle.is_focused(window) {
                        window.blur();
                    }
                })
                .child(Input::new(&p.name)),
        )
    } else {
        let name_value = p.name.read(cx).value();
        let header_label: SharedString = if name_value.trim().is_empty() {
            i18n::t("settings-models-unnamed")
        } else {
            name_value
        };
        let label = div().text_sm().truncate().child(header_label);
        let label = if selected {
            label
                .text_color(theme.info)
                .font_weight(gpui::FontWeight::MEDIUM)
        } else {
            label.font_weight(gpui::FontWeight::MEDIUM)
        };
        toggle.child(label)
    };

    let header_row = h_flex()
        .w_full()
        .items_center()
        .gap_2()
        .child(toggle)
        .child({
            let key = format!("models-p{pid}-remove");
            let pending = panel.pending_delete.as_deref() == Some(key.as_str());
            remove_button(
                theme,
                key,
                pending,
                entity.clone(),
                Arc::new(move |this, cx| {
                    this.models_panel.remove_provider(pid);
                    this.models_panel.touch(cx);
                }),
            )
        })
        .into_any_element();

    let mut children: Vec<AnyElement> = vec![header_row];
    if expanded {
        for module in MODULE_TABS {
            let active = selected && panel.module == module;
            let nav_entity = entity.clone();
            let row = div()
                .id(format!("models-p{pid}-nav-{module:?}"))
                .pl_6()
                .pr_2()
                .py_1()
                .rounded(theme.radius)
                .cursor_pointer()
                .text_sm();
            let row = if active {
                row.text_color(theme.info)
                    .font_weight(gpui::FontWeight::MEDIUM)
            } else {
                row.hover(|s| s.bg(theme.accent.opacity(0.06)))
            };
            children.push(
                row.on_click(move |_ev, _window, cx| {
                    nav_entity.update(cx, |this, cx| {
                        this.models_panel.selected = Some(pid);
                        this.models_panel.module = module;
                        cx.notify();
                    });
                })
                .child(
                    h_flex()
                        .w_full()
                        .items_center()
                        .justify_between()
                        .child(module_label(module))
                        .child(if active {
                            Icon::new(IconName::ChevronRight)
                                .small()
                                .text_color(theme.info)
                                .into_any_element()
                        } else {
                            div().into_any_element()
                        }),
                )
                .into_any_element(),
            );
        }
    }
    v_flex().w_full().children(children).into_any_element()
}

/// 基本信息 module: the API Key kind dropdown + value input pair (the name
/// lives in the card header, edited via double-click).
fn render_basic_module(
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

    let kind_control = h_flex()
        .w_full()
        .gap_2()
        .child(token_dropdown(
            format!("models-p{pid}-apikey-kind"),
            SharedString::from(kind_token(p.apikey_kind)),
            kind_options,
            entity.clone(),
            Arc::new(move |this, v, window, cx| {
                if let Some(p) = this.models_panel.provider_mut(pid) {
                    p.apikey_kind = kind_from_token(&v);
                    let placeholder = kind_placeholder(p.apikey_kind);
                    p.apikey_value.update(cx, |state, cx| {
                        state.set_placeholder(placeholder, window, cx)
                    });
                    this.models_panel.touch(cx);
                }
            }),
        ))
        .child(
            div()
                .id(format!(
                    "settings-input-blur-{:?}",
                    p.apikey_value.entity_id()
                ))
                .flex_1()
                .min_w_0()
                .on_mouse_down_out(blur_on_click_out(p.apikey_value.clone()))
                .child(Input::new(&p.apikey_value)),
        )
        .into_any_element();

    v_flex()
        .w_full()
        .child(
            h_flex().px_3().py_1p5().child(
                div()
                    .text_sm()
                    .text_color(theme.foreground)
                    .child(i18n::t("settings-models-row-apikey")),
            ),
        )
        .child(h_flex().w_full().px_3().py_1p5().child(kind_control))
        .into_any_element()
}

/// 环境变量 key/value block: plain label, indented rows, each row trailed by
/// 「-」(delete row) and 「+」(insert a focused empty row below). With no rows
/// a muted hint plus a 「+」 offers the first row.
fn render_env_block(
    theme: &Theme,
    entity: Entity<SettingsView>,
    pid: usize,
    mid: Option<usize>,
    rows: &[KvForm],
    pending: &Option<String>,
) -> AnyElement {
    let mut children: Vec<AnyElement> = vec![
        h_flex()
            .px_3()
            .py_1p5()
            .child(
                div()
                    .text_sm()
                    .text_color(theme.foreground)
                    .child(i18n::t("settings-models-section-env")),
            )
            .into_any_element(),
    ];

    if rows.is_empty() {
        children.push(
            h_flex()
                .px_3()
                .py_1()
                .items_center()
                .gap_2()
                .child(muted_text(
                    i18n::t("settings-models-env-empty"),
                    theme.muted_foreground,
                ))
                .child(icon_button(
                    format!("models-p{pid}-m{mid:?}-env-add-first"),
                    IconName::Plus,
                    entity.clone(),
                    Arc::new(move |this, window, cx| {
                        this.models_panel.add_env_row(window, cx, pid, mid, None);
                        this.models_panel.touch(cx);
                    }),
                ))
                .into_any_element(),
        );
    }

    for (ix, kv) in rows.iter().enumerate() {
        let kid = kv.id;
        children.push(
            h_flex()
                .px_3()
                .py_1()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .id(format!("settings-input-blur-{:?}", kv.key.entity_id()))
                        .w(px(KEY_W))
                        .flex_shrink_0()
                        .on_mouse_down_out(blur_on_click_out(kv.key.clone()))
                        .child(Input::new(&kv.key)),
                )
                .child(
                    div()
                        .id(format!("settings-input-blur-{:?}", kv.value.entity_id()))
                        .flex_1()
                        .min_w_0()
                        .on_mouse_down_out(blur_on_click_out(kv.value.clone()))
                        .child(Input::new(&kv.value)),
                )
                .child({
                    let key = format!("models-kv{kid}-remove");
                    let is_pending = pending.as_deref() == Some(key.as_str());
                    remove_button(
                        theme,
                        key,
                        is_pending,
                        entity.clone(),
                        Arc::new(move |this, cx| {
                            this.models_panel.remove_env_row(pid, mid, kid);
                            this.models_panel.touch(cx);
                        }),
                    )
                })
                .child(icon_button(
                    format!("models-kv{kid}-add-below"),
                    IconName::Plus,
                    entity.clone(),
                    Arc::new(move |this, window, cx| {
                        this.models_panel
                            .add_env_row(window, cx, pid, mid, Some(ix));
                        this.models_panel.touch(cx);
                    }),
                ))
                .into_any_element(),
        );
    }
    v_flex().w_full().children(children).into_any_element()
}

/// 端点信息 module: add button first (hidden once all three wire APIs are
/// taken), then one card per endpoint.
fn render_endpoints_module(
    theme: &Theme,
    entity: Entity<SettingsView>,
    pid: usize,
    p: &ProviderForm,
    agent_options: &[String],
    pending: &Option<String>,
) -> AnyElement {
    let used: HashSet<String> = p.endpoints.iter().map(|e| e.wire_api.to_string()).collect();

    let mut children: Vec<AnyElement> = Vec::new();
    for e in &p.endpoints {
        children.push(render_endpoint(
            theme,
            entity.clone(),
            pid,
            &used,
            agent_options,
            e,
            pending,
        ));
    }
    if used.len() < WIRE_APIS.len() {
        children.push(dashed_add_button(
            theme,
            format!("models-p{pid}-add-endpoint"),
            "settings-models-add-endpoint",
            entity.clone(),
            Arc::new(move |this, window, cx| {
                this.models_panel.add_endpoint(window, cx, pid);
                this.models_panel.touch(cx);
            }),
        ));
    }
    v_flex()
        .w_full()
        .gap_2()
        .children(children)
        .into_any_element()
}

fn render_endpoint(
    theme: &Theme,
    entity: Entity<SettingsView>,
    pid: usize,
    used: &HashSet<String>,
    agent_options: &[String],
    e: &EndpointForm,
    pending: &Option<String>,
) -> AnyElement {
    let eid = e.id;
    let current = e.wire_api.to_string();
    // Each wire API may appear at most once per provider: a row offers its
    // own kind plus the kinds no other row has taken.
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

    let wire_control = token_dropdown(
        format!("models-e{eid}-wire"),
        e.wire_api.clone(),
        wire_options,
        entity.clone(),
        Arc::new(move |this, v, _window, cx| {
            if let Some(e) = this.models_panel.endpoint_mut(pid, eid) {
                e.wire_api = v;
                this.models_panel.touch(cx);
            }
        }),
    );

    let mut rows = vec![
        field_row(
            theme,
            i18n::t("settings-models-row-wire-apis"),
            wire_control,
        ),
        field_row(
            theme,
            i18n::t("settings-models-row-url"),
            input_field(&e.url),
        ),
        field_row(
            theme,
            i18n::t("settings-models-row-agents"),
            agent_badges(
                format!("models-e{eid}-agents"),
                agent_options,
                &e.agents,
                entity.clone(),
                Arc::new(move |this, v, cx| {
                    if let Some(e) = this.models_panel.endpoint_mut(pid, eid) {
                        e.agents = v;
                        this.models_panel.touch(cx);
                    }
                }),
            ),
        ),
    ];
    if agents_include_copilot(&e.agents) {
        rows.push(field_row(
            theme,
            i18n::t("settings-models-row-copilot"),
            token_dropdown(
                format!("models-e{eid}-copilot"),
                e.copilot_auth.clone(),
                copilot_options,
                entity.clone(),
                Arc::new(move |this, v, _window, cx| {
                    if let Some(e) = this.models_panel.endpoint_mut(pid, eid) {
                        e.copilot_auth = v;
                        this.models_panel.touch(cx);
                    }
                }),
            ),
        ));
    }
    let card = plain_card(theme, rows);
    let key = format!("models-e{eid}-remove");
    let is_pending = pending.as_deref() == Some(key.as_str());
    h_flex()
        .w_full()
        .items_center()
        .gap_2()
        .child(div().flex_1().min_w_0().child(card))
        .child(remove_button(
            theme,
            key,
            is_pending,
            entity.clone(),
            Arc::new(move |this, cx| {
                this.models_panel.remove_endpoint(pid, eid);
                this.models_panel.touch(cx);
            }),
        ))
        .into_any_element()
}

/// 模型列表 module: 手动配置 / 自动获取 tab; manual mode renders one card per
/// model (blocks, not hairline-separated rows).
fn render_models_module(
    theme: &Theme,
    entity: Entity<SettingsView>,
    pid: usize,
    p: &ProviderForm,
    agent_options: &[String],
    pending: &Option<String>,
) -> AnyElement {
    let mut children: Vec<AnyElement> = vec![render_mode_tabs(theme, entity.clone(), pid, p)];
    if p.remote_models {
        children.push(field_row(
            theme,
            i18n::t("settings-models-row-url"),
            input_field(&p.remote_url),
        ));
    } else {
        if p.models.is_empty() {
            children.push(
                h_flex()
                    .px_3()
                    .py_2()
                    .child(muted_text(
                        i18n::t("settings-models-empty-models"),
                        theme.muted_foreground,
                    ))
                    .into_any_element(),
            );
        }
        for m in &p.models {
            children.push(render_model(
                theme,
                entity.clone(),
                pid,
                m,
                agent_options,
                pending,
            ));
        }
        children.push(dashed_add_button(
            theme,
            format!("models-p{pid}-add-model"),
            "settings-models-add-model",
            entity.clone(),
            Arc::new(move |this, window, cx| {
                this.models_panel.add_model(window, cx, pid);
                this.models_panel.touch(cx);
            }),
        ));
    }
    v_flex()
        .w_full()
        .gap_2()
        .children(children)
        .into_any_element()
}

/// Two-choice tab replacing the old 内联定义 / 远程 URL button pair; the
/// active tab is underlined and tinted with `theme.info` so it reads clearly.
fn render_mode_tabs(
    theme: &Theme,
    entity: Entity<SettingsView>,
    pid: usize,
    p: &ProviderForm,
) -> AnyElement {
    let muted = theme.muted_foreground;
    let mut tabs = Vec::new();
    for (ix, (active, label_key)) in [
        (!p.remote_models, "settings-models-mode-inline"),
        (p.remote_models, "settings-models-mode-remote"),
    ]
    .into_iter()
    .enumerate()
    {
        let tab_entity = entity.clone();
        let remote = ix == 1;
        let tab = div()
            .id(format!("models-p{pid}-mode-{ix}"))
            .px_2()
            .py_1()
            .border_b_2()
            .cursor_pointer()
            .text_sm();
        let tab = if active {
            tab.text_color(theme.info)
                .border_color(theme.info)
                .font_weight(gpui::FontWeight::MEDIUM)
        } else {
            tab.text_color(muted)
                .border_color(gpui::transparent_black())
                .hover(|s| s.text_color(theme.foreground))
        };
        tabs.push(
            tab.on_click(move |_ev, _window, cx| {
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
            .child(i18n::t(label_key))
            .into_any_element(),
        );
    }
    h_flex()
        .px_3()
        .py_1()
        .gap_2()
        .children(tabs)
        .into_any_element()
}

fn render_model(
    theme: &Theme,
    entity: Entity<SettingsView>,
    pid: usize,
    m: &ModelForm,
    agent_options: &[String],
    pending: &Option<String>,
) -> AnyElement {
    let mid = m.id;

    let card = plain_card(
        theme,
        vec![
            field_row(
                theme,
                i18n::t("settings-models-row-model-id"),
                input_field(&m.model_id),
            ),
            field_row(
                theme,
                i18n::t("settings-models-row-desc"),
                input_field(&m.desc),
            ),
            field_row(
                theme,
                i18n::t("settings-models-row-context"),
                input_field(&m.context),
            ),
            field_row(
                theme,
                i18n::t("settings-models-row-max-tokens"),
                input_field(&m.max_tokens),
            ),
            field_row(
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
            ),
            field_row(
                theme,
                i18n::t("settings-models-row-agents"),
                agent_badges(
                    format!("models-m{mid}-agents"),
                    agent_options,
                    &m.agents,
                    entity.clone(),
                    Arc::new(move |this, v, cx| {
                        if let Some(m) = this.models_panel.model_mut(pid, mid) {
                            m.agents = v;
                            this.models_panel.touch(cx);
                        }
                    }),
                ),
            ),
            field_row(
                theme,
                i18n::t("settings-models-row-supports-tools"),
                bool_dropdown(
                    format!("models-m{mid}-tools"),
                    m.supports_tools,
                    entity.clone(),
                    Arc::new(move |this, v, _window, cx| {
                        if let Some(m) = this.models_panel.model_mut(pid, mid) {
                            m.supports_tools = v == "true";
                            this.models_panel.touch(cx);
                        }
                    }),
                ),
            ),
            field_row(
                theme,
                i18n::t("settings-models-row-supports-images"),
                bool_dropdown(
                    format!("models-m{mid}-images"),
                    m.supports_images,
                    entity.clone(),
                    Arc::new(move |this, v, _window, cx| {
                        if let Some(m) = this.models_panel.model_mut(pid, mid) {
                            m.supports_images = v == "true";
                            this.models_panel.touch(cx);
                        }
                    }),
                ),
            ),
            render_env_block(theme, entity.clone(), pid, Some(mid), &m.env, pending),
        ],
    );
    let key = format!("models-m{mid}-remove");
    let is_pending = pending.as_deref() == Some(key.as_str());
    h_flex()
        .w_full()
        .items_center()
        .gap_2()
        .child(div().flex_1().min_w_0().child(card))
        .child(remove_button(
            theme,
            key,
            is_pending,
            entity.clone(),
            Arc::new(move |this, cx| {
                this.models_panel.remove_model(pid, mid);
                this.models_panel.touch(cx);
            }),
        ))
        .into_any_element()
}

// --- Element builders -----------------------------------------------------

fn plain_card(theme: &Theme, children: Vec<AnyElement>) -> AnyElement {
    v_flex()
        .w_full()
        .p_2()
        .gap_0()
        .rounded(px(10.))
        .border_1()
        .border_color(theme.border.opacity(0.5))
        .children(children)
        .into_any_element()
}

/// Label-left / control-right row for form fields; every module row goes
/// through here so labels and controls share one grid.
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
    div()
        .id(format!("settings-input-blur-{:?}", state.entity_id()))
        .w_full()
        .on_mouse_down_out(blur_on_click_out(state.clone()))
        .child(Input::new(state))
        .into_any_element()
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
                menu.item(
                    PopupMenuItem::new(display.clone()).on_click(move |_ev, window, cx| {
                        entity.update(cx, |this, cx| {
                            apply(this, token.clone(), window, cx);
                            cx.notify();
                        });
                    }),
                )
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

/// Selected agents as removable badges plus a dropdown that adds the
/// remaining candidates — replaces the old click-to-toggle chip row.
fn agent_badges(
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

    let mut children: Vec<AnyElement> = Vec::new();
    for tag in selected {
        let tag_clone = tag.clone();
        let selected_vec = selected.to_vec();
        let entity = entity.clone();
        let apply = apply.clone();
        children.push(
            h_flex()
                .items_center()
                .gap_1()
                .pl_2()
                .pr_1()
                .py_0p5()
                .rounded(px(6.))
                .bg(gpui::hsla(0., 0., 0.5, 0.08))
                .text_xs()
                .child(SharedString::from(agent_display_name(tag)))
                .child(
                    Button::new(format!("{id_prefix}-rm-{tag}"))
                        .icon(Icon::new(IconName::Close).small())
                        .ghost()
                        .small()
                        .on_click(move |_ev, _window, cx| {
                            let next: Vec<String> = selected_vec
                                .iter()
                                .filter(|t| *t != &tag_clone)
                                .cloned()
                                .collect();
                            entity.update(cx, |this, cx| {
                                apply(this, next.clone(), cx);
                                cx.notify();
                            });
                        }),
                )
                .into_any_element(),
        );
    }

    let remaining: Vec<String> = all
        .iter()
        .filter(|t| !selected.contains(*t))
        .cloned()
        .collect();
    let add_button = Button::new(format!("{id_prefix}-add"))
        .label(i18n::t("settings-models-agents-add"))
        .small()
        .outline()
        .dropdown_caret(true);
    let selected_vec = selected.to_vec();
    children.push(
        add_button
            .dropdown_menu_with_anchor(Anchor::BottomRight, move |menu, _window, _cx| {
                remaining.iter().fold(menu, |menu, tag| {
                    let tag = tag.clone();
                    let entity = entity.clone();
                    let apply = apply.clone();
                    let selected_vec = selected_vec.clone();
                    menu.item(
                        PopupMenuItem::new(SharedString::from(agent_display_name(&tag))).on_click(
                            move |_ev, _window, cx| {
                                let mut next = selected_vec.clone();
                                next.push(tag.clone());
                                entity.update(cx, |this, cx| {
                                    apply(this, next.clone(), cx);
                                    cx.notify();
                                });
                            },
                        ),
                    )
                })
            })
            .into_any_element(),
    );
    if selected.is_empty() {
        children.push(muted_text(
            i18n::t("settings-models-agents-all-hint"),
            gpui::transparent_black(),
        ));
    }
    h_flex()
        .w_full()
        .flex_wrap()
        .items_center()
        .gap_1()
        .children(children)
        .into_any_element()
}

fn icon_button(
    id: impl Into<gpui::ElementId>,
    icon: IconName,
    entity: Entity<SettingsView>,
    apply: WindowApply,
) -> AnyElement {
    Button::new(id)
        .icon(Icon::new(icon))
        .small()
        .ghost()
        .on_click(move |_ev, window, cx| {
            entity.update(cx, |this, cx| {
                apply(this, window, cx);
                cx.notify();
            });
        })
        .into_any_element()
}

fn dashed_add_button(
    theme: &Theme,
    id: impl Into<gpui::ElementId>,
    label_key: &'static str,
    entity: Entity<SettingsView>,
    apply: WindowApply,
) -> AnyElement {
    let border = theme.border.opacity(0.9);
    let muted = theme.muted_foreground;
    let accent = theme.accent;
    div()
        .id(id)
        .w_full()
        .py_2()
        .rounded(px(8.))
        .border_1()
        .border_dashed()
        .border_color(border)
        .cursor_pointer()
        .hover(move |s| s.border_color(accent).text_color(accent))
        .on_click(move |_ev, window, cx| {
            entity.update(cx, |this, cx| {
                apply(this, window, cx);
                cx.notify();
            });
        })
        .child(
            h_flex()
                .w_full()
                .items_center()
                .justify_center()
                .gap_1()
                .text_sm()
                .text_color(muted)
                .child(Icon::new(IconName::Plus).small())
                .child(i18n::t(label_key)),
        )
        .into_any_element()
}

/// Uniform 「-」 remove control with two-step confirmation: the first click
/// arms it (danger tint), the second executes the deletion.
fn remove_button(
    theme: &Theme,
    id: String,
    pending: bool,
    entity: Entity<SettingsView>,
    apply: MutApply,
) -> AnyElement {
    let key = id.clone();
    let icon = Icon::new(IconName::Minus);
    let icon = if pending {
        icon.text_color(theme.danger)
    } else {
        icon.text_color(theme.muted_foreground)
    };
    let button = Button::new(id).icon(icon).ghost();
    let button = if pending {
        button.bg(theme.danger.opacity(0.12))
    } else {
        button
    };
    button
        .on_click(move |_ev, _window, cx| {
            entity.update(cx, |this, cx| {
                if this.models_panel.pending_delete.as_deref() == Some(key.as_str()) {
                    this.models_panel.pending_delete = None;
                    apply(this, cx);
                } else {
                    this.models_panel.pending_delete = Some(key.clone());
                }
                cx.notify();
            });
        })
        .into_any_element()
}

/// Carry over top-level sections this form does not model (owned by the
/// External Tools panels and the host's subagent dispatch) from a fresh disk
/// read, so a Models-panel save can never erase them.
fn carry_over_unmodeled_sections(config: &mut CxConfig, fresh: &CxConfig) {
    config.chatgpt_app = fresh.chatgpt_app.clone();
    config.vscode_app = fresh.vscode_app.clone();
    config.subagents = fresh.subagents.clone();
}

#[cfg(test)]
mod tests {
    use super::carry_over_unmodeled_sections;
    use cx_providers::{ChatGptAppSettings, CxConfig, VsCodeAppSettings, VsCodeExtensionBlock};

    /// Regression: the CxConfig produced by the Models panel's `collect()`
    /// carries empty values for all three unmodeled sections; the carry-over
    /// must restore `chatgpt_app` / `vscode_app` / `subagents` from the fresh
    /// disk read, or autosave silently erases the External Tools panels'
    /// settings and the host's subagent model pinning.
    #[test]
    fn carry_over_preserves_unmodeled_sections() {
        let mut collected = CxConfig::default();
        let fresh = CxConfig {
            providers: Vec::new(),
            agents: Vec::new(),
            chatgpt_app: Some(ChatGptAppSettings::default()),
            vscode_app: Some(VsCodeAppSettings {
                claude_code: VsCodeExtensionBlock {
                    provider: Some("百炼".into()),
                    disabled: false,
                },
                codex: VsCodeExtensionBlock {
                    provider: None,
                    disabled: true,
                },
            }),
            subagents: [(
                "Explore".to_string(),
                "百炼::qwen3.8-flash::high".to_string(),
            )]
            .into_iter()
            .collect(),
        };
        carry_over_unmodeled_sections(&mut collected, &fresh);
        assert!(collected.chatgpt_app.is_some());
        let vscode = collected
            .vscode_app
            .expect("vscode_app must survive carry-over");
        assert_eq!(vscode.claude_code.provider.as_deref(), Some("百炼"));
        assert!(vscode.codex.disabled);
        assert_eq!(
            collected.subagents.get("Explore").map(String::as_str),
            Some("百炼::qwen3.8-flash::high"),
            "subagents must survive carry-over"
        );
    }
}

#[cfg(test)]
mod blur_flush_tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use gpui::{AppContext as _, Entity, ParentElement as _, Render, TestAppContext, div};
    use gpui_component::Root;
    use gpui_component::input::{Input, InputEvent, InputState};

    struct ProbeView {
        input: Entity<InputState>,
    }

    impl Render for ProbeView {
        fn render(
            &mut self,
            _window: &mut gpui::Window,
            _cx: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            div().child(Input::new(&self.input))
        }
    }

    /// The settings panels flush pending edits when an input blurs. The flush
    /// is wired in an `InputEvent::Blur` subscription, whose dispatch leases
    /// the view entity for the whole callback; the flush itself needs
    /// `&mut Window`, which subscriptions do not receive, so
    /// `panels::defer_blur_flush` re-enters the view through the saved window
    /// handle at the end of the effect cycle, after the dispatch lease is
    /// released (a synchronous re-entry would double-lease the entity and
    /// panic). This test drives a real focus→blur cycle through gpui's draw
    /// focus phase and pins that deferred re-entry behavior.
    #[gpui::test]
    fn blur_flush_reentry_runs_after_focus_loss(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let flushed = Rc::new(Cell::new(false));
        let flushed_for_view = flushed.clone();
        let slot: Rc<std::cell::RefCell<Option<Entity<ProbeView>>>> = Default::default();
        let slot_for_window = slot.clone();
        let (_root, cx) = cx.add_window_view(move |window, cx| {
            let view = cx.new(|cx| {
                let window_handle = window.window_handle();
                let input = cx.new(|cx| InputState::new(window, cx));
                // Same shape as the `new_input`/`panel_input` Blur arm.
                let flushed = flushed_for_view.clone();
                cx.subscribe(&input, move |_this, _state, event, cx| {
                    if matches!(event, InputEvent::Blur) {
                        let flushed = flushed.clone();
                        super::super::panels::defer_blur_flush(
                            cx,
                            window_handle,
                            move |_this, _window, _cx| {
                                flushed.set(true);
                            },
                        );
                    }
                })
                .detach();
                ProbeView { input }
            });
            *slot_for_window.borrow_mut() = Some(view.clone());
            Root::new(view, window, cx)
        });
        let view = slot.borrow().clone().expect("view initialized");
        let input = view.read_with(cx, |view, _| view.input.clone());

        // Focus change events only dispatch while the window is active.
        cx.update(|window, _cx| window.activate_window());
        cx.run_until_parked();

        cx.update(|window, cx| {
            input.update(cx, |input, cx| input.focus(window, cx));
        });
        cx.run_until_parked();

        cx.update(|window, _cx| window.blur());
        cx.run_until_parked();

        assert!(
            flushed.get(),
            "deferred blur flush never ran after focus loss"
        );
    }
}
