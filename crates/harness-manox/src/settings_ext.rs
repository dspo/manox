//! Side-call model selection for the retired manox harness (needs the
//! manox model registry; the pi harness resolves its own models).

use std::sync::OnceLock;

use agent::settings::{SideCallPolicy, SideCallsSettings, load};

/// An unavailable override warns once per model reference.
pub fn side_call_model(
    policy: &SideCallPolicy,
    main: &crate::language_model::AnyLanguageModel,
) -> crate::language_model::AnyLanguageModel {
    if policy.model.trim().is_empty() {
        return main.clone();
    }
    if let Some(model) = crate::model_alias::resolve_model_ref(policy.model.trim()) {
        return model;
    }
    static WARNED: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    let warned = WARNED.get_or_init(Default::default);
    let mut warned = warned.lock().unwrap();
    if warned.insert(policy.model.clone()) {
        tracing::warn!(
            model = policy.model,
            fallback = main.id(),
            "side-call model unavailable; falling back to main model"
        );
    }
    main.clone()
}

/// The settings snapshot's side-call policies (manox-harness convenience).
pub fn side_calls() -> SideCallsSettings {
    load().side_calls
}

pub fn claude_md_load_context() -> crate::claude_md::LoadContext {
    #[cfg(test)]
    let ctx = crate::claude_md::LoadContext::default();
    #[cfg(not(test))]
    let ctx = crate::claude_md::LoadContext {
        home: agent::paths::home_dir(),
        managed: crate::claude_md::managed_policy_path(),
        excludes: claude_md_excludes(),
        // External imports stay withheld until the approval flow (a later PR)
        // wires the persisted per-anchor decision in here.
        allow_external: false,
    };
    ctx
}

/// The session-cached `claude_md_excludes` list. Lazily settled on first use
/// so `agent::init` ordering does not matter; a mid-session settings edit
/// takes effect next launch, matching the modes snapshot.
#[cfg(not(test))]
fn claude_md_excludes() -> Vec<String> {
    static EXCLUDES: OnceLock<Vec<String>> = OnceLock::new();
    EXCLUDES.get_or_init(|| load().claude_md_excludes).clone()
}
