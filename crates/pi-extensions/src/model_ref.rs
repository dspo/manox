//! Model-reference resolution over the pi provider registry.
//!
//! Agent/command frontmatter commonly pins `model: sonnet` (or `opus` /
//! `haiku`), assuming a Claude Code runtime backed by Anthropic. The cx
//! providers config connects to arbitrary providers, so a literal `sonnet`
//! id rarely resolves. This layer bridges that assumption:
//!
//! 1. An exact model id match wins outright.
//! 2. Otherwise a Claude/OpenAI alias table maps the ref to a segment
//!    probe against the registered model list — `sonnet` → any model whose
//!    id has a path segment whose first hyphen/dot/underscore token is
//!    `sonnet`.
//! 3. As a last resort, the ref itself is used as a segment probe.
//!
//! First-token matching (not raw substring) avoids false positives: `o3`
//! does not match `proto3-server` (its first token is `proto3`), and
//! `sonnet` does not match `crimsonsonnet-x`. Falls back to `None` when
//! nothing matches, in which case the caller applies its own fallback
//! (e.g. the first registered model).

/// `(alias, segment_probe)` pairs. The probe must be the first hyphen/dot/
/// underscore-delimited token of a live model id segment (case-insensitive),
/// so `o3` matches `o3-mini` but not `proto3-server`, and `sonnet` matches
/// `sonnet-4` but not `crimsonsonnet-x`.
pub const ALIASES: &[(&str, &str)] = &[
    ("claude-sonnet", "sonnet"),
    ("claude-opus", "opus"),
    ("claude-haiku", "haiku"),
    ("sonnet", "sonnet"),
    ("opus", "opus"),
    ("haiku", "haiku"),
    ("gpt-4o", "gpt-4o"),
    ("gpt-5", "gpt-5"),
    ("o3", "o3"),
];

/// True when `id` has a `/`- or `:`-delimited segment whose first `-`/`.`/`_`
/// token equals `probe` (case-insensitive). First-token equality — not raw
/// substring containment — so `o3` matches `anthropic/o3-mini` (token `o3`)
/// but not `proto3-server` (token `proto3`), and `sonnet` matches `sonnet-4`
/// but not `crimsonsonnet-x`.
pub fn matches_segment(id: &str, probe: &str) -> bool {
    let probe = probe.to_lowercase();
    id.to_lowercase()
        .split(['/', ':'])
        .any(|seg| segment_first_token(seg) == probe)
}

/// The leading sub-token of a model segment, splitting on `-`, `.`, and `_`.
pub fn segment_first_token(seg: &str) -> &str {
    seg.split(['-', '.', '_']).next().unwrap_or("")
}

/// Resolve a model reference against a provider registry: an exact model id
/// wins outright, then the alias table maps to a segment probe
/// (case-insensitive on the alias), and the ref itself probes as a last
/// resort.
pub fn resolve_model_ref(
    registry: &pi::ProviderRegistry,
    model_ref: &str,
) -> Option<pi::types::Model> {
    let models = registry.models();
    // Exact match is case-insensitive like the alias/probe paths, so
    // `DeepSeek-V4-Flash` and `deepseek-v4-flash` resolve identically.
    let wanted = model_ref.to_lowercase();
    if let Some(exact) = models.iter().find(|m| m.id.to_lowercase() == wanted) {
        return Some(exact.clone());
    }
    let probe = ALIASES
        .iter()
        .find(|(alias, _)| *alias == model_ref.to_lowercase())
        .map(|(_, probe)| *probe)
        .unwrap_or(model_ref);
    models.into_iter().find(|m| matches_segment(&m.id, probe))
}

/// Agent visibility from registration metadata: missing metadata (a
/// registration without domain visibility notes) means visible; otherwise
/// the effective agent list must contain `agent_id`.
pub fn visible_to_agent(model: &pi::types::Model, agent_id: &str) -> bool {
    model
        .metadata
        .get("agents")
        .and_then(|v| v.as_array())
        .map(|list| list.iter().any(|a| a.as_str() == Some(agent_id)))
        .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi::provider_registry::{Cost, ProviderConfig, ProviderModelConfig};
    use std::collections::HashMap;

    fn register(registry: &pi::ProviderRegistry, id: &str, agents: Option<serde_json::Value>) {
        let mut metadata = HashMap::new();
        if let Some(v) = agents {
            metadata.insert("agents".to_string(), v);
        }
        registry
            .register_provider(
                &format!("p-{id}"),
                ProviderConfig {
                    name: Some("P".into()),
                    base_url: Some("https://p.example".into()),
                    api_key: Some("k".into()),
                    api: Some(pi::provider_registry::Api::AnthropicMessages),
                    headers: None,
                    auth_header: false,
                    models: vec![ProviderModelConfig {
                        id: id.into(),
                        name: id.into(),
                        reasoning: false,
                        input: vec![pi::provider_registry::InputModality::Text],
                        context_window: 1000,
                        max_tokens: 100,
                        cost: Cost::default(),
                        api: None,
                        base_url: None,
                        metadata,
                    }],
                },
            )
            .unwrap();
    }

    #[test]
    fn exact_id_wins() {
        let registry = pi::ProviderRegistry::new();
        register(&registry, "deepseek-v4-flash", None);
        let m = resolve_model_ref(&registry, "deepseek-v4-flash").unwrap();
        assert_eq!(m.id, "deepseek-v4-flash");
    }

    #[test]
    fn alias_is_case_insensitive_and_probes_first_token() {
        let registry = pi::ProviderRegistry::new();
        register(&registry, "crimsonsonnet-x", None);
        register(&registry, "sonnet-4-6", None);
        // `Sonnet` must hit `sonnet-4-6` (segment starts with the probe)
        // and never `crimsonsonnet-x` (first-token rule, not substring).
        let m = resolve_model_ref(&registry, "Sonnet").unwrap();
        assert_eq!(m.id, "sonnet-4-6");
        let m = resolve_model_ref(&registry, "CLAUDE-SONNET").unwrap();
        assert_eq!(m.id, "sonnet-4-6");
        // A model whose first token differs is invisible to the alias.
        register(&registry, "claude-sonnet-4-6", None);
        let registry2 = pi::ProviderRegistry::new();
        register(&registry2, "claude-sonnet-4-6", None);
        assert!(resolve_model_ref(&registry2, "sonnet").is_none());
    }

    #[test]
    fn exact_match_is_case_insensitive() {
        let registry = pi::ProviderRegistry::new();
        register(&registry, "deepseek-v4-flash", None);
        assert_eq!(
            resolve_model_ref(&registry, "DeepSeek-V4-Flash")
                .unwrap()
                .id,
            "deepseek-v4-flash"
        );
    }

    #[test]
    fn alias_table_covers_claude_and_openai_families() {
        // Alias lookup is exact equality on the (lowercased) ref, so table
        // order cannot shadow entries — pin the per-alias probes.
        let probe = |r: &str| {
            ALIASES
                .iter()
                .find(|(alias, _)| *alias == r.to_lowercase())
                .map(|(_, p)| *p)
        };
        assert_eq!(probe("claude-sonnet"), Some("sonnet"));
        assert_eq!(probe("claude-opus"), Some("opus"));
        assert_eq!(probe("claude-haiku"), Some("haiku"));
        assert_eq!(probe("sonnet"), Some("sonnet"));
        assert_eq!(probe("o3"), Some("o3"));
        assert_eq!(probe("gpt-5"), Some("gpt-5"));
        assert_eq!(probe("unknown-ref"), None);
    }

    #[test]
    fn bare_ref_probes_and_misses_cleanly() {
        let registry = pi::ProviderRegistry::new();
        register(&registry, "proto3-server", None);
        assert!(resolve_model_ref(&registry, "o3").is_none());
        register(&registry, "o3-mini", None);
        assert_eq!(resolve_model_ref(&registry, "o3").unwrap().id, "o3-mini");
    }

    #[test]
    fn visibility_reads_agents_metadata() {
        use serde_json::json;
        let registry = pi::ProviderRegistry::new();
        register(&registry, "open-model", None);
        register(&registry, "claude-only", Some(json!(["claude"])));
        register(&registry, "codex-only", Some(json!(["codex+"])));
        register(&registry, "hidden", Some(json!([])));
        let model = |id: &str| registry.models().into_iter().find(|m| m.id == id).unwrap();
        // Missing metadata (no visibility notes) is visible to everyone.
        assert!(visible_to_agent(&model("open-model"), "claude"));
        assert!(visible_to_agent(&model("claude-only"), "claude"));
        assert!(!visible_to_agent(&model("codex-only"), "claude"));
        // An empty effective list hides the model from every agent.
        assert!(!visible_to_agent(&model("hidden"), "claude"));
    }
}
