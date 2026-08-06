//! The process-wide pi provider registry.
//!
//! Registration is owned by the `pi_extensions::provider` extension, which
//! reads the native cx providers config and registers every provider
//! endpoint into the kernel [`pi::ProviderRegistry`]. The host only
//! initializes, reloads, and hands out the shared snapshot — the actor
//! side resolves models and streams through it.

use std::sync::{Arc, OnceLock, RwLock};

use pi::ProviderRegistry;

static REGISTRY: OnceLock<RwLock<Arc<ProviderRegistry>>> = OnceLock::new();

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
/// which must not block the gpui main thread. Early consumers see an empty
/// snapshot until the build lands; the pi actor reloads before first use,
/// and the model menu simply lists nothing until then.
pub fn init() {
    let _ = REGISTRY.set(RwLock::new(Arc::new(ProviderRegistry::new())));
    std::thread::spawn(|| {
        let fresh = build();
        if let Some(lock) = REGISTRY.get() {
            *lock.write().unwrap_or_else(|e| e.into_inner()) = fresh;
        }
    });
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
        if let Some(model) = crate::model_alias::resolve_pi_model_ref(r) {
            return Some(model);
        }
        tracing::warn!(reference = %r, "default_model did not resolve; falling back to first registered model");
    }
    let models = global().models();
    models
        .iter()
        .find(|m| visible_to_agent(m, "claude"))
        .cloned()
        .or_else(|| models.into_iter().next())
}

/// Agent visibility from registration metadata: missing metadata (a
/// non-cx registration) means visible; otherwise the effective agent list
/// must contain `agent_id`.
fn visible_to_agent(model: &pi::types::Model, agent_id: &str) -> bool {
    model
        .metadata
        .get("agents")
        .and_then(|v| v.as_array())
        .map(|list| list.iter().any(|a| a.as_str() == Some(agent_id)))
        .unwrap_or(true)
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
    use std::collections::HashMap;

    fn model_with_agents(agents: Option<serde_json::Value>) -> pi::types::Model {
        let mut metadata = HashMap::new();
        if let Some(v) = agents {
            metadata.insert("agents".to_string(), v);
        }
        pi::types::Model {
            provider: "p".into(),
            api: "anthropic".into(),
            id: "m".into(),
            context_window: 1,
            max_tokens: 1,
            thinking: pi::types::ThinkingKind::None,
            metadata,
        }
    }

    #[test]
    fn visibility_reads_agents_metadata() {
        use serde_json::json;
        // Missing metadata (non-cx registration) is visible to everyone.
        assert!(visible_to_agent(&model_with_agents(None), "claude"));
        // Membership decides.
        assert!(visible_to_agent(
            &model_with_agents(Some(json!(["claude", "codex+"]))),
            "claude"
        ));
        assert!(!visible_to_agent(
            &model_with_agents(Some(json!(["codex+"]))),
            "claude"
        ));
        // An empty effective list hides the model from every agent.
        assert!(!visible_to_agent(
            &model_with_agents(Some(json!([]))),
            "claude"
        ));
    }
}
