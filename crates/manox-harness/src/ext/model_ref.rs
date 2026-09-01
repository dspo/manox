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
    registry: &crate::core::ProviderRegistry,
    model_ref: &str,
) -> Option<crate::core::types::Model> {
    let models = registry.models();
    // Exact match is case-insensitive like the alias/probe paths, so
    // `DeepSeek-V4-Flash` and `deepseek-v4-flash` resolve identically.
    let wanted = model_ref.to_lowercase();
    // A registration-qualified reference (`provider/id`) pins one wire
    // endpoint: wire variants of one model share the bare id, so the
    // bare-id match below would always resolve the first registration.
    if let Some(exact) = models
        .iter()
        .find(|m| format!("{}/{}", m.provider, m.id).to_lowercase() == wanted)
    {
        return Some(exact.clone());
    }
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

/// A parsed `provider::model::effort` spec — the value of a dedicated
/// subagent-model config entry. The effort part is optional.
#[derive(Debug)]
pub struct ModelSpec {
    pub provider: String,
    pub model: String,
    pub effort: Option<String>,
}

/// A spec resolved against the registry: the model plus the optional effort
/// tier (absent when the spec carried none).
#[derive(Debug)]
pub struct ResolvedModelSpec {
    pub model: crate::core::types::Model,
    pub effort: Option<String>,
}

/// Split `provider::model` / `provider::model::effort` on `::`. Exactly two
/// or three parts; the effort, when present, is a wire thinking level —
/// `off` disables thinking, `low`/`medium`/`high`/`max` set the tier.
pub fn parse_model_spec(spec: &str) -> Result<ModelSpec, String> {
    let parts: Vec<&str> = spec.split("::").map(str::trim).collect();
    let (provider, model, effort) = match parts.as_slice() {
        [p, m] => (*p, *m, None),
        [p, m, e] => (*p, *m, Some(*e)),
        _ => {
            return Err(format!(
                "expected `provider::model` or `provider::model::effort`, got `{spec}`"
            ));
        }
    };
    if provider.is_empty() || model.is_empty() {
        return Err(format!("provider and model must be non-empty: `{spec}`"));
    }
    if let Some(e) = effort
        && !matches!(e, "off" | "low" | "medium" | "high" | "max")
    {
        return Err(format!(
            "unknown effort `{e}` (expected off|low|medium|high|max)"
        ));
    }
    Ok(ModelSpec {
        provider: provider.to_string(),
        model: model.to_string(),
        effort: effort.map(str::to_string),
    })
}

/// Resolve a `provider::model::effort` spec: the provider is matched by
/// registration name or metadata `provider_display_name`; the model by exact
/// id (case-insensitive) then first-token segment probe, scoped to that
/// provider's models. The alias table (`sonnet`/`haiku`/`opus`) is
/// deliberately not applied — those are not provider-scoped concepts. `Err`
/// is loud so the caller fails closed instead of silently falling back.
pub fn resolve_model_spec(
    registry: &crate::core::ProviderRegistry,
    spec: &str,
) -> Result<ResolvedModelSpec, String> {
    let parsed = parse_model_spec(spec)?;
    let scoped: Vec<crate::core::types::Model> = registry
        .models()
        .into_iter()
        .filter(|m| {
            m.provider == parsed.provider
                || m.metadata
                    .get("provider_display_name")
                    .and_then(|v| v.as_str())
                    == Some(parsed.provider.as_str())
        })
        .collect();
    if scoped.is_empty() {
        return Err(format!(
            "no provider `{}` (by name or display name)",
            parsed.provider
        ));
    }
    let wanted = parsed.model.to_lowercase();
    let model = scoped
        .iter()
        .find(|m| m.id.to_lowercase() == wanted)
        .or_else(|| {
            scoped
                .iter()
                .find(|m| matches_segment(&m.id, &parsed.model))
        })
        .cloned()
        .ok_or_else(|| {
            format!(
                "model `{}` not found under provider `{}`",
                parsed.model, parsed.provider
            )
        })?;
    Ok(ResolvedModelSpec {
        model,
        effort: parsed.effort,
    })
}

/// Agent visibility from registration metadata: missing metadata (a
/// registration without domain visibility notes) means visible; otherwise
/// the effective agent list must contain `agent_id`.
pub fn visible_to_agent(model: &crate::core::types::Model, agent_id: &str) -> bool {
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
    use crate::core::provider_registry::{Cost, ProviderConfig, ProviderModelConfig};
    use std::collections::HashMap;

    fn register(
        registry: &crate::core::ProviderRegistry,
        id: &str,
        agents: Option<serde_json::Value>,
    ) {
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
                    api: Some(crate::core::provider_registry::Api::AnthropicMessages),
                    headers: None,
                    auth_header: false,
                    models: vec![ProviderModelConfig {
                        id: id.into(),
                        name: id.into(),
                        reasoning: false,
                        input: vec![crate::core::provider_registry::InputModality::Text],
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
        let registry = crate::core::ProviderRegistry::new();
        register(&registry, "deepseek-v4-flash", None);
        let m = resolve_model_ref(&registry, "deepseek-v4-flash").unwrap();
        assert_eq!(m.id, "deepseek-v4-flash");
    }

    #[test]
    fn alias_is_case_insensitive_and_probes_first_token() {
        let registry = crate::core::ProviderRegistry::new();
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
        let registry2 = crate::core::ProviderRegistry::new();
        register(&registry2, "claude-sonnet-4-6", None);
        assert!(resolve_model_ref(&registry2, "sonnet").is_none());
    }

    #[test]
    fn registration_qualified_ref_pins_wire_variant() {
        let mut metadata = HashMap::new();
        metadata.insert(
            "provider_display_name".to_string(),
            serde_json::json!("DeepSeek"),
        );
        let model = ProviderModelConfig {
            id: "deepseek-v4-pro".into(),
            name: "deepseek-v4-pro".into(),
            reasoning: false,
            input: vec![crate::core::provider_registry::InputModality::Text],
            context_window: 1000,
            max_tokens: 100,
            cost: Cost::default(),
            api: None,
            base_url: None,
            metadata: metadata.clone(),
        };
        let registry = crate::core::ProviderRegistry::new();
        registry
            .register_provider(
                "DeepSeek-anthropic",
                ProviderConfig {
                    name: Some("DeepSeek".into()),
                    base_url: Some("https://api.example/anthropic".into()),
                    api_key: Some("k".into()),
                    api: Some(crate::core::provider_registry::Api::AnthropicMessages),
                    headers: None,
                    auth_header: false,
                    models: vec![model.clone()],
                },
            )
            .unwrap();
        registry
            .register_provider(
                "DeepSeek-responses",
                ProviderConfig {
                    name: Some("DeepSeek".into()),
                    base_url: Some("https://api.example".into()),
                    api_key: Some("k".into()),
                    api: Some(crate::core::provider_registry::Api::OpenAiResponses),
                    headers: None,
                    auth_header: false,
                    models: vec![model],
                },
            )
            .unwrap();
        // A bare id resolves the first registration (anthropic).
        let bare = resolve_model_ref(&registry, "deepseek-v4-pro").unwrap();
        assert_eq!(bare.provider, "DeepSeek-anthropic");
        assert_eq!(bare.api, "anthropic");
        // A registration-qualified ref pins the responses endpoint.
        let pinned = resolve_model_ref(&registry, "DeepSeek-responses/deepseek-v4-pro").unwrap();
        assert_eq!(pinned.provider, "DeepSeek-responses");
        assert_eq!(pinned.api, "openai_responses");
        // An unknown registration in a qualified ref resolves to nothing
        // (fails closed) instead of silently picking the first variant.
        assert!(resolve_model_ref(&registry, "no-such-provider/deepseek-v4-pro").is_none());
    }

    #[test]
    fn exact_match_is_case_insensitive() {
        let registry = crate::core::ProviderRegistry::new();
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
        let registry = crate::core::ProviderRegistry::new();
        register(&registry, "proto3-server", None);
        assert!(resolve_model_ref(&registry, "o3").is_none());
        register(&registry, "o3-mini", None);
        assert_eq!(resolve_model_ref(&registry, "o3").unwrap().id, "o3-mini");
    }

    #[test]
    fn visibility_reads_agents_metadata() {
        use serde_json::json;
        let registry = crate::core::ProviderRegistry::new();
        register(&registry, "open-model", None);
        register(&registry, "claude-only", Some(json!(["claude"])));
        register(&registry, "codex-only", Some(json!(["codex"])));
        register(&registry, "hidden", Some(json!([])));
        let model = |id: &str| registry.models().into_iter().find(|m| m.id == id).unwrap();
        // Missing metadata (no visibility notes) is visible to everyone.
        assert!(visible_to_agent(&model("open-model"), "claude"));
        assert!(visible_to_agent(&model("claude-only"), "claude"));
        assert!(!visible_to_agent(&model("codex-only"), "claude"));
        // An empty effective list hides the model from every agent.
        assert!(!visible_to_agent(&model("hidden"), "claude"));
    }

    fn register_displayed(
        registry: &crate::core::ProviderRegistry,
        reg_name: &str,
        display: &str,
        id: &str,
    ) {
        registry
            .register_provider(
                reg_name,
                ProviderConfig {
                    name: Some(display.into()),
                    base_url: Some("https://p.example".into()),
                    api_key: Some("k".into()),
                    api: Some(crate::core::provider_registry::Api::AnthropicMessages),
                    headers: None,
                    auth_header: false,
                    models: vec![ProviderModelConfig {
                        id: id.into(),
                        name: id.into(),
                        reasoning: false,
                        input: vec![crate::core::provider_registry::InputModality::Text],
                        context_window: 1000,
                        max_tokens: 100,
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
    fn parse_model_spec_shapes() {
        let s = parse_model_spec("DeepSeek::deepseek-v4-flash::high").unwrap();
        assert_eq!(s.provider, "DeepSeek");
        assert_eq!(s.model, "deepseek-v4-flash");
        assert_eq!(s.effort.as_deref(), Some("high"));
        // The effort part is optional, and parts are trimmed.
        let s = parse_model_spec(" 百炼 :: glm-5.3 ").unwrap();
        assert_eq!(s.provider, "百炼");
        assert_eq!(s.model, "glm-5.3");
        assert_eq!(s.effort, None);
        // `off` pins the subagent to no thinking; the other tiers are the
        // wire effort levels.
        let s = parse_model_spec("DeepSeek::deepseek-v4-flash::off").unwrap();
        assert_eq!(s.effort.as_deref(), Some("off"));
        let err = parse_model_spec("a::b::c::d").unwrap_err();
        assert!(
            err.contains("expected `provider::model` or `provider::model::effort`"),
            "{err}"
        );
        let err = parse_model_spec("::m::high").unwrap_err();
        assert!(
            err.contains("provider and model must be non-empty"),
            "{err}"
        );
        let err = parse_model_spec("p::m::ultra").unwrap_err();
        assert!(err.contains("unknown effort `ultra`"), "{err}");
    }

    #[test]
    fn resolve_model_spec_scopes_by_display_or_registration_name() {
        let registry = crate::core::ProviderRegistry::new();
        register_displayed(
            &registry,
            "DeepSeek-anthropic",
            "DeepSeek",
            "deepseek-v4-pro",
        );
        register_displayed(
            &registry,
            "DeepSeek-responses",
            "DeepSeek",
            "deepseek-v4-pro",
        );
        register_displayed(&registry, "Other-anthropic", "Other", "deepseek-v4-pro");
        // Display-name scoping pins the first registration of that provider.
        let by_display = resolve_model_spec(&registry, "DeepSeek::deepseek-v4-pro::high").unwrap();
        assert_eq!(by_display.model.provider, "DeepSeek-anthropic");
        assert_eq!(by_display.effort.as_deref(), Some("high"));
        // Registration-name scoping pins the exact endpoint.
        let by_reg = resolve_model_spec(&registry, "DeepSeek-responses::deepseek-v4-pro").unwrap();
        assert_eq!(by_reg.model.provider, "DeepSeek-responses");
        assert_eq!(by_reg.effort, None);
        // A different display name resolves its own registration.
        let other = resolve_model_spec(&registry, "Other::deepseek-v4-pro").unwrap();
        assert_eq!(other.model.provider, "Other-anthropic");
    }

    #[test]
    fn resolve_model_spec_probes_segments_and_fails_loudly() {
        let registry = crate::core::ProviderRegistry::new();
        register(&registry, "sonnet-4-6", None);
        // No alias table — the ref probes as a segment token; exact ids are
        // matched case-insensitively.
        let probed = resolve_model_spec(&registry, "P::sonnet").unwrap();
        assert_eq!(probed.model.id, "sonnet-4-6");
        let exact = resolve_model_spec(&registry, "P::SONNET-4-6").unwrap();
        assert_eq!(exact.model.id, "sonnet-4-6");
        let err = resolve_model_spec(&registry, "Nope::m").unwrap_err();
        assert!(
            err.contains("no provider `Nope` (by name or display name)"),
            "{err}"
        );
        let err = resolve_model_spec(&registry, "P::no-such-model").unwrap_err();
        assert!(
            err.contains("model `no-such-model` not found under provider `P`"),
            "{err}"
        );
        let err = resolve_model_spec(&registry, "P::sonnet::ultra").unwrap_err();
        assert!(err.contains("unknown effort"), "{err}");
    }
}
