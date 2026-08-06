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

#[cfg(test)]
mod tests {
    use super::*;
    use pi::coding_agent::model_runtime::ModelCatalog;
    use pi::provider_registry::{Api, Cost, ProviderConfig, ProviderModelConfig};

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
}
