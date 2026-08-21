//! The process-wide pi provider registry.
//!
//! Registration is owned by the `pi_extensions::provider` extension, which
//! reads the native cx providers config and registers every provider
//! endpoint into the kernel [`pi::ProviderRegistry`]. The host only
//! initializes, reloads, and hands out the shared snapshot — the actor
//! side resolves models and streams through it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

use pi::ProviderRegistry;

static REGISTRY: OnceLock<RwLock<Arc<ProviderRegistry>>> = OnceLock::new();
/// Signalled once the initial background registration completes (success or
/// soft failure). Actors await this instead of triggering their own reload,
/// so registration runs exactly once per process.
static READY: OnceLock<tokio::sync::Notify> = OnceLock::new();
static READY_FLAG: AtomicBool = AtomicBool::new(false);

fn build() -> Arc<ProviderRegistry> {
    let registry = Arc::new(ProviderRegistry::new());
    match pi_extensions::provider::register_providers(
        &registry,
        pi_extensions::provider::default_config_path(),
    ) {
        Ok(0) => tracing::info!("pi providers: no provider endpoints registered"),
        Ok(count) => tracing::info!("pi providers: registered {count} provider endpoints"),
        Err(err) => tracing::warn!("pi providers: registration failed: {err:#}"),
    }
    registry
}

/// Initialize the shared registry at app startup. Soft-fail: a broken or
/// missing config yields an empty registry plus a warning — never a
/// startup panic.
///
/// Registration runs on a background thread: it may hit the OS keychain
/// (including interactive unlock prompts) or run `$(...)` shell commands,
/// which must not block the gpui main thread. Consumers that must not see
/// an empty snapshot await [`wait_ready`]; the model menu simply lists
/// nothing until the build lands.
pub fn init() {
    let _ = REGISTRY.set(RwLock::new(Arc::new(ProviderRegistry::new())));
    let notify = READY.get_or_init(tokio::sync::Notify::new);
    std::thread::spawn(move || {
        let fresh = build();
        if let Some(lock) = REGISTRY.get() {
            *lock.write().unwrap_or_else(|e| e.into_inner()) = fresh;
        }
        READY_FLAG.store(true, Ordering::Release);
        notify.notify_waiters();
    });
}

/// Wait for the one-shot initial registration to finish. Returns at once
/// when it already completed, or when [`init`] was never called (nothing
/// to wait for). Pi actors await this once at startup instead of
/// triggering per-thread provider reloads — registration (and its
/// keychain/shell cost) happens exactly once per process.
pub async fn wait_ready() {
    let Some(notify) = READY.get() else {
        return;
    };
    // Register interest BEFORE re-checking the flag so a completion that
    // lands in between cannot be missed.
    let notified = notify.notified();
    if READY_FLAG.load(Ordering::Acquire) {
        return;
    }
    notified.await;
}

/// The current registry snapshot. Cheap `Arc` clone; actors hold their
/// snapshot for life, so a later `reload` only affects threads spawned
/// afterwards (same semantics as the manox registry).
pub fn global() -> Arc<ProviderRegistry> {
    // A poisoned lock (a panic mid-swap) should degrade to the last good
    // snapshot, not cascade into every model-listing view.
    REGISTRY
        .get()
        .expect("pi_providers not initialized; call agent::init first")
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// Rebuild from the config file and atomically swap the snapshot. The
/// previous snapshot is kept on failure. Blocking (keychain / shell
/// commands) — call from a background thread.
pub fn reload() -> anyhow::Result<()> {
    let fresh = Arc::new(ProviderRegistry::new());
    let count = pi_extensions::provider::register_providers(
        &fresh,
        pi_extensions::provider::default_config_path(),
    )?;
    tracing::info!("pi providers: reloaded {count} provider endpoints");
    let lock = REGISTRY
        .get()
        .expect("pi_providers not initialized; call agent::init first");
    *lock.write().unwrap_or_else(|e| e.into_inner()) = fresh;
    Ok(())
}

/// The default model for new pi threads: the settings `default_model`
/// reference (id or alias) resolved against the registry, else the first
/// registered model (sorted) that is visible to claude-class agents (pi is
/// claude-class), else the first registered model outright, else `None`.
pub fn default_model() -> Option<pi::types::Model> {
    let reference = crate::settings::load().default_model;
    if let Some(r) = reference
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
    {
        if let Some(model) = pi_extensions::model_ref::resolve_model_ref(&global(), r) {
            return Some(model);
        }
        tracing::warn!(reference = %r, "default_model did not resolve; falling back to first registered model");
    }
    let models = global().models();
    models
        .iter()
        .find(|m| pi_extensions::model_ref::visible_to_agent(m, "claude"))
        .cloned()
        .or_else(|| models.into_iter().next())
}

/// Resolve a side-call model override (title generation). A
/// non-empty `policy.model` reference (id or alias) resolved against the
/// registry wins; an empty or unresolvable reference yields `None` so the
/// caller inherits the session model — matching the `side_calls` contract
/// ("Empty → inherit the main model").
pub fn resolve_side_call_model(
    policy: &crate::settings::SideCallPolicy,
) -> Option<pi::types::Model> {
    resolve_side_call_model_in(&global(), policy)
}

/// Registry-injectable core of [`resolve_side_call_model`] (testable
/// without the process-global registry).
pub fn resolve_side_call_model_in(
    registry: &pi::ProviderRegistry,
    policy: &crate::settings::SideCallPolicy,
) -> Option<pi::types::Model> {
    let reference = policy.model.trim();
    if reference.is_empty() {
        return None;
    }
    let resolved = pi_extensions::model_ref::resolve_model_ref(registry, reference);
    if resolved.is_none() {
        tracing::warn!(
            reference = %reference,
            "side-call model override did not resolve; inheriting the session model"
        );
    }
    resolved
}
/// Model catalog restoring legacy manox-style provider ids persisted by
/// older sessions: `{wire}:{name}` (e.g. `anthropic:DeepSeek`) aliases the
/// current registration name `{name}-{wire}` (`DeepSeek-anthropic`). A
/// host-side compatibility layer — the kernel catalog only knows
/// registration names.
pub struct LegacyAliasCatalog {
    registry: Arc<ProviderRegistry>,
}

impl LegacyAliasCatalog {
    pub fn new(registry: Arc<ProviderRegistry>) -> Self {
        Self { registry }
    }
}

impl pi::coding_agent::model_runtime::ModelCatalog for LegacyAliasCatalog {
    fn resolve(&self, provider: &str, model_id: &str) -> Option<pi::types::Model> {
        let catalog = self.registry.catalog();
        if let Some(model) = catalog.resolve(provider, model_id) {
            return Some(model);
        }
        if let Some((wire, name)) = provider.split_once(':') {
            let aliased = format!("{name}-{wire}");
            return catalog.resolve(&aliased, model_id);
        }
        None
    }
}

/// Display name of a registered model (metadata `name`, else the id).
pub fn display_name(model: &pi::types::Model) -> String {
    model
        .metadata
        .get("name")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| model.id.clone())
}

/// Display name of a model's provider (metadata `provider_display_name`,
/// else the registration name).
pub fn display_provider_name(model: &pi::types::Model) -> String {
    model
        .metadata
        .get("provider_display_name")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| model.provider.clone())
}

/// Config-level identity of a registered model (metadata `config_id`,
/// else the registration id). Host model lists key rows by this so a model
/// registered through several wire apis appears once.
pub fn config_id(model: &pi::types::Model) -> String {
    model
        .metadata
        .get("config_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| model.id.clone())
}

/// Cx wire key of a registered model's endpoint (`"anthropic"` /
/// `"responses"` / `"completions"`); `None` for non-cx registrations.
pub fn wire_key(model: &pi::types::Model) -> Option<&'static str> {
    pi_extensions::provider::model_api_to_wire_key(&model.api)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi::coding_agent::model_runtime::ModelCatalog;
    use pi::provider_registry::{Api, Cost, ProviderConfig, ProviderModelConfig};

    #[test]
    fn config_id_prefers_metadata_over_registration_id() {
        let model = |metadata| pi::types::Model {
            provider: "bailian-a".into(),
            api: "anthropic".into(),
            id: "deepseek-v4-flash".into(),
            context_window: 200_000,
            max_tokens: 8_192,
            thinking: pi::types::ThinkingKind::None,
            metadata,
        };
        assert_eq!(
            config_id(&model(std::collections::HashMap::new())),
            "deepseek-v4-flash"
        );
        assert_eq!(
            config_id(&model(std::collections::HashMap::from([(
                "config_id".to_string(),
                serde_json::json!("bailian:deepseek-v4-flash"),
            )]))),
            "bailian:deepseek-v4-flash"
        );
        // Non-string metadata values fall back to the registration id.
        assert_eq!(
            config_id(&model(std::collections::HashMap::from([(
                "config_id".to_string(),
                serde_json::json!(42),
            )]))),
            "deepseek-v4-flash"
        );
    }

    #[test]
    fn legacy_alias_catalog_resolves_old_provider_ids() {
        let registry = Arc::new(ProviderRegistry::new());
        registry
            .register_provider(
                "DeepSeek-anthropic",
                ProviderConfig {
                    name: Some("DeepSeek".into()),
                    base_url: Some("https://api.deepseek.com/anthropic".into()),
                    api_key: Some("k".into()),
                    api: Some(Api::AnthropicMessages),
                    headers: None,
                    auth_header: false,
                    models: vec![ProviderModelConfig {
                        id: "deepseek-v4-flash".into(),
                        name: "deepseek-v4-flash".into(),
                        reasoning: false,
                        input: vec![],
                        context_window: 1_000_000,
                        max_tokens: 8_192,
                        cost: Cost::default(),
                        api: None,
                        base_url: None,
                        metadata: std::collections::HashMap::new(),
                    }],
                },
            )
            .unwrap();
        let catalog = LegacyAliasCatalog::new(registry);
        // Current registration names resolve directly.
        assert!(
            catalog
                .resolve("DeepSeek-anthropic", "deepseek-v4-flash")
                .is_some()
        );
        // Legacy manox-style ids alias onto them.
        assert!(
            catalog
                .resolve("anthropic:DeepSeek", "deepseek-v4-flash")
                .is_some()
        );
        // Unknown ids fall through to the default catalog chain.
        assert!(catalog.resolve("anthropic", "claude-sonnet-4-6").is_some());
        assert!(catalog.resolve("anthropic:DeepSeek", "nope").is_none());
    }

    fn register_model(registry: &ProviderRegistry, id: &str) {
        registry
            .register_provider(
                &format!("p-{id}"),
                ProviderConfig {
                    name: Some("P".into()),
                    base_url: Some("https://p.example".into()),
                    api_key: Some("k".into()),
                    api: Some(Api::AnthropicMessages),
                    headers: None,
                    auth_header: false,
                    models: vec![ProviderModelConfig {
                        id: id.into(),
                        name: id.into(),
                        reasoning: false,
                        input: vec![],
                        context_window: 1_000_000,
                        max_tokens: 8_192,
                        cost: Cost::default(),
                        api: None,
                        base_url: None,
                        metadata: std::collections::HashMap::new(),
                    }],
                },
            )
            .unwrap();
    }

    #[test]
    fn side_call_override_empty_inherits() {
        let registry = ProviderRegistry::new();
        register_model(&registry, "haiku");
        // An empty override (the default) inherits the session model.
        let policy = crate::settings::SideCallPolicy::default();
        assert!(resolve_side_call_model_in(&registry, &policy).is_none());
    }

    #[test]
    fn side_call_override_resolves_reference() {
        let registry = ProviderRegistry::new();
        register_model(&registry, "haiku");
        let policy = crate::settings::SideCallPolicy {
            model: "haiku".into(),
            ..Default::default()
        };
        let resolved = resolve_side_call_model_in(&registry, &policy).unwrap();
        assert_eq!(resolved.id, "haiku");
    }

    #[test]
    fn side_call_override_unresolvable_inherits() {
        let registry = ProviderRegistry::new();
        register_model(&registry, "haiku");
        // A reference that matches nothing falls back to the session model.
        let policy = crate::settings::SideCallPolicy {
            model: "no-such-model".into(),
            ..Default::default()
        };
        assert!(resolve_side_call_model_in(&registry, &policy).is_none());
    }
}
