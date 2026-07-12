//! Process-global `LanguageModel` registry.
//!
//! `init(cx)` reads the cx yaml → builds the matching `LanguageModel` for each
//! `ResolvedModel` by `wire_api` → stores them in a global
//! `Vec<Arc<dyn LanguageModel>>`. `list_models` / `get_model` serve the UI and `Thread`.

use std::sync::{Arc, OnceLock};

use gpui::App;

use crate::language_model::AnyLanguageModel;
use crate::provider::anthropic::AnthropicModel;
use crate::provider::completions::CompletionsModel;
use crate::provider::resolve_apikey;
use crate::provider::responses::ResponsesModel;
use crate::provider::{CxConfig, ResolvedModel, WireApi};

/// Global provider registry.
static REGISTRY: OnceLock<ProviderRegistry> = OnceLock::new();

pub struct ProviderRegistry {
    models: Vec<AnyLanguageModel>,
}

impl ProviderRegistry {
    /// Build models from the default cx config, resolving each api_key.
    /// Models whose api_key fails to resolve are skipped (warn-logged, never blocking the rest).
    pub fn from_config(config: CxConfig) -> Self {
        let mut models: Vec<AnyLanguageModel> = Vec::new();
        for resolved in config.resolve_all_models() {
            match build_model(&resolved) {
                Ok(m) => models.push(m),
                Err(e) => tracing::warn!(
                    provider = resolved.provider_name.as_str(),
                    model = resolved.id.as_str(),
                    error = %e,
                    "Skipping unresolvable model"
                ),
            }
        }
        Self { models }
    }

    pub fn models(&self) -> &[AnyLanguageModel] {
        &self.models
    }

    /// Look up a model by its stable manox id (`provider/model/wire`).
    pub fn get_model(&self, id: &str) -> Option<AnyLanguageModel> {
        self.models.iter().find(|m| m.id() == id).cloned()
    }
}

/// Build a concrete `LanguageModel` from a `ResolvedModel` by `wire_api`. Requires resolving the api_key.
fn build_model(resolved: &ResolvedModel) -> anyhow::Result<AnyLanguageModel> {
    let api_key = resolve_apikey(resolved.apikey_source.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "provider {} has no apikey_source configured",
            resolved.provider_name
        )
    })?)?;
    let max_tokens = resolve_context_window(resolved);
    let auto_compact_window =
        resolve_auto_compact_window(resolved.wire_api, &resolved.env, max_tokens);

    let model: AnyLanguageModel = match resolved.wire_api {
        WireApi::Anthropic => Arc::new(AnthropicModel::new(
            resolved.key(),
            resolved.id.clone(),
            resolved.provider_name.clone(),
            resolved.api_model_id(),
            resolved.endpoint_url.clone(),
            api_key,
            max_tokens,
            auto_compact_window,
        )),
        WireApi::Responses => Arc::new(ResponsesModel::new(
            resolved.key(),
            resolved.id.clone(),
            resolved.provider_name.clone(),
            resolved.api_model_id(),
            resolved.endpoint_url.clone(),
            api_key,
            max_tokens,
        )),
        WireApi::Completions => Arc::new(CompletionsModel::new(
            resolved.key(),
            resolved.id.clone(),
            resolved.provider_name.clone(),
            resolved.api_model_id(),
            resolved.endpoint_url.clone(),
            api_key,
            max_tokens,
        )),
        WireApi::Unavailable => {
            anyhow::bail!("wire_api {:?} is unavailable", resolved.wire_api)
        }
    };
    Ok(model)
}

/// Ceiling for a model's `max_output_tokens` request field. A model's context
/// budget (e.g. glm-5.2 `[1m]` = 1,000,000, decimal) far exceeds what any provider lets a
/// single response emit, so the raw `max_token_count` is unsuitable as the
/// output budget. Capped to keep responses bounded; floored well above the old
/// hard `8192` so a reasoning model mid-thinking is not cut off mid-tool-call —
/// a truncated `tool_use` input JSON stalls the turn (see thread 76aef71a).
pub(crate) const MAX_OUTPUT_TOKENS_CAP: u64 = 32_768;
const MIN_OUTPUT_TOKENS: u64 = 8_192;

pub(crate) fn default_max_output_tokens(max_token_count: u64) -> u64 {
    max_token_count.clamp(MIN_OUTPUT_TOKENS, MAX_OUTPUT_TOKENS_CAP)
}

/// Resolve a model's context-window size in tokens. The `context` yaml field
/// takes precedence when present and parseable; an unparseable `context` is a
/// hard error (warn + 8192 fallback) — it does NOT silently fall through to a
/// bracket suffix on the id, because an explicit field means the operator chose
/// a value and a typo should surface, not masquerade as a different number. When
/// no `context` is set, a trailing bracket suffix on the id (e.g.
/// `glm-5.2[1m123k]`) is parsed; absent both, 8192.
fn resolve_context_window(resolved: &ResolvedModel) -> u64 {
    let ctx = resolved.context.trim();
    if !ctx.is_empty() {
        match cx_providers::parse_context_window(ctx) {
            Some(n) => n,
            None => {
                tracing::warn!(
                    model_id = %resolved.id,
                    context = %resolved.context,
                    "context field unparseable; fallback 8192, no bracket fallback"
                );
                8192
            }
        }
    } else {
        cx_providers::context_window_from_suffix(&resolved.id).unwrap_or(8192)
    }
}
/// Provider-config env var (`~/.config/cx/cx.providers.config.yaml`, provider-
/// level or model-level `env:`) that overrides a model's auto-compact trigger
/// window. Only effective on the Anthropic wire; see `build_model`. When set,
/// the thread auto-compacts at 80% of the parsed token count instead of the
/// model's full `max_token_count` at the settings threshold — Claude Code parity.
const CLAUDE_CODE_AUTO_COMPACT_WINDOW_ENV: &str = "CLAUDE_CODE_AUTO_COMPACT_WINDOW";

/// Parse `CLAUDE_CODE_AUTO_COMPACT_WINDOW` from a resolved model's env map.
/// The value is a plain integer token count (e.g. `"202745"`) — no `k`/`m`
/// unit suffixes, matching Claude Code. A non-positive or unparseable value is
/// warn-logged and ignored so a typo never silently shrinks the window.
fn auto_compact_window_from_env(env: &std::collections::BTreeMap<String, String>) -> Option<u64> {
    let raw = env.get(CLAUDE_CODE_AUTO_COMPACT_WINDOW_ENV)?;
    match raw.trim().parse::<u64>() {
        Ok(n) if n > 0 => Some(n),
        Ok(_) => {
            tracing::warn!(
                env = CLAUDE_CODE_AUTO_COMPACT_WINDOW_ENV,
                value = raw.as_str(),
                "auto-compact window must be a positive integer; ignoring override"
            );
            None
        }
        Err(_) => {
            tracing::warn!(
                env = CLAUDE_CODE_AUTO_COMPACT_WINDOW_ENV,
                value = raw.as_str(),
                "auto-compact window is not a valid integer; ignoring override"
            );
            None
        }
    }
}

/// Resolve the auto-compact window override for a model. Encapsulates three
/// guards: (1) the env var only takes effect on the Anthropic wire; (2) a
/// value at or above `max_token_count` would make compaction unreachable
/// (threshold = 80% of a window larger than the real context budget); (3) a
/// value below `MIN_COMPACTION_CONTEXT_WINDOW` is accepted by the parser but
/// silently discarded by the compaction floor guard. All three are warn-logged
/// so a misconfiguration surfaces rather than silently disabling compaction.
fn resolve_auto_compact_window(
    wire_api: WireApi,
    env: &std::collections::BTreeMap<String, String>,
    max_token_count: u64,
) -> Option<u64> {
    if wire_api != WireApi::Anthropic {
        if env.contains_key(CLAUDE_CODE_AUTO_COMPACT_WINDOW_ENV) {
            tracing::warn!(
                env = CLAUDE_CODE_AUTO_COMPACT_WINDOW_ENV,
                wire_api = ?wire_api,
                "CLAUDE_CODE_AUTO_COMPACT_WINDOW is only effective on the Anthropic wire; ignoring"
            );
        }
        return None;
    }
    let window = auto_compact_window_from_env(env)?;
    if window >= max_token_count {
        tracing::warn!(
            env = CLAUDE_CODE_AUTO_COMPACT_WINDOW_ENV,
            value = window,
            max_token_count,
            "auto-compact window >= max_token_count; the 80% threshold may never be reached — compaction could be silently disabled"
        );
    }
    if window < crate::compact::MIN_COMPACTION_CONTEXT_WINDOW {
        tracing::warn!(
            env = CLAUDE_CODE_AUTO_COMPACT_WINDOW_ENV,
            value = window,
            min = crate::compact::MIN_COMPACTION_CONTEXT_WINDOW,
            "auto-compact window below MIN_COMPACTION_CONTEXT_WINDOW; compaction will be disabled by the floor guard"
        );
    }
    Some(window)
}

/// Read the cx config, build the registry, and register it globally. Call at App startup.
/// Panics on failure — manox is unusable without an LLM.
pub fn init(_cx: &mut App) {
    let config = CxConfig::load_default()
        .unwrap_or_else(|e| panic!("Failed to load cx providers config: {e}"));
    let registry = ProviderRegistry::from_config(config);
    if registry.models().is_empty() {
        tracing::error!("ProviderRegistry initialized with no available models");
    }
    let _ = REGISTRY.set(registry);
}

/// Returns the global registry. Panics if `init` was not called.
pub fn global() -> &'static ProviderRegistry {
    REGISTRY
        .get()
        .expect("ProviderRegistry not initialized; call agent::init first")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str, context: &str) -> ResolvedModel {
        ResolvedModel {
            id: id.to_string(),
            swe_pro: String::new(),
            hle: String::new(),
            desc: String::new(),
            context: context.to_string(),
            wire_api: WireApi::Anthropic,
            model_wire_apis: Vec::new(),
            provider_name: String::new(),
            endpoint_url: String::new(),
            visible_agents: Vec::new(),
            copilot_auth: cx_providers::CopilotAuth::ApiKey,
            env: std::collections::BTreeMap::new(),
            apikey_source: None,
        }
    }

    #[test]
    fn resolve_context_window_cases() {
        // context field wins over a bracket suffix on the id.
        assert_eq!(
            resolve_context_window(&model("glm-5.2[1m]", "244k")),
            244_000
        );
        // bracket suffix when no context field.
        assert_eq!(resolve_context_window(&model("glm-5.2[1m]", "")), 1_000_000);
        assert_eq!(
            resolve_context_window(&model("glm-5.2[1m1234k]", "")),
            2_234_000
        );
        // unparseable context is a hard error: 8192, no bracket fallback.
        assert_eq!(
            resolve_context_window(&model("glm-5.2[1m]", "garbage")),
            8192
        );
        // neither context nor suffix.
        assert_eq!(resolve_context_window(&model("glm-5.2", "")), 8192);
        // context is trimmed before parsing.
        assert_eq!(resolve_context_window(&model("glm-5.2", " 1m ")), 1_000_000);
    }

    #[test]
    fn default_max_output_tokens_cases() {
        // Floor: a tiny or zero context budget still gets a usable output window.
        assert_eq!(default_max_output_tokens(0), MIN_OUTPUT_TOKENS);
        assert_eq!(default_max_output_tokens(4_096), MIN_OUTPUT_TOKENS);
        // In-range: passes through up to the cap.
        assert_eq!(default_max_output_tokens(16_384), 16_384);
        assert_eq!(
            default_max_output_tokens(MAX_OUTPUT_TOKENS_CAP),
            MAX_OUTPUT_TOKENS_CAP
        );
        // Cap: a 1m-context model is bounded, not handed a million-token budget.
        assert_eq!(default_max_output_tokens(200 * 1024), MAX_OUTPUT_TOKENS_CAP);
        assert_eq!(
            default_max_output_tokens(1024 * 1024),
            MAX_OUTPUT_TOKENS_CAP
        );
    }
    #[test]
    fn auto_compact_window_from_env_parses_and_falls_back() {
        use std::collections::BTreeMap;
        let mut env = BTreeMap::new();
        // Absent → None.
        assert_eq!(auto_compact_window_from_env(&env), None);
        // Plain integer → Some.
        env.insert(
            CLAUDE_CODE_AUTO_COMPACT_WINDOW_ENV.to_string(),
            "202745".to_string(),
        );
        assert_eq!(auto_compact_window_from_env(&env), Some(202_745));
        // Whitespace-tolerant.
        env.insert(
            CLAUDE_CODE_AUTO_COMPACT_WINDOW_ENV.to_string(),
            "  100000  ".to_string(),
        );
        assert_eq!(auto_compact_window_from_env(&env), Some(100_000));
        // Garbage → None (warn).
        env.insert(
            CLAUDE_CODE_AUTO_COMPACT_WINDOW_ENV.to_string(),
            "not-a-number".to_string(),
        );
        assert_eq!(auto_compact_window_from_env(&env), None);
        // Zero → None (warn).
        env.insert(
            CLAUDE_CODE_AUTO_COMPACT_WINDOW_ENV.to_string(),
            "0".to_string(),
        );
        assert_eq!(auto_compact_window_from_env(&env), None);
        // Unit suffix rejected — plain integer only, matching Claude Code.
        env.insert(
            CLAUDE_CODE_AUTO_COMPACT_WINDOW_ENV.to_string(),
            "200k".to_string(),
        );
        assert_eq!(auto_compact_window_from_env(&env), None);
    }

    #[test]
    fn resolve_auto_compact_window_gates_by_wire_api() {
        use std::collections::BTreeMap;
        let mut env = BTreeMap::new();
        env.insert(
            CLAUDE_CODE_AUTO_COMPACT_WINDOW_ENV.to_string(),
            "202745".to_string(),
        );
        // Anthropic wire → Some.
        assert_eq!(
            resolve_auto_compact_window(WireApi::Anthropic, &env, 1_000_000),
            Some(202_745)
        );
        // Responses wire → None (env var ignored).
        assert_eq!(
            resolve_auto_compact_window(WireApi::Responses, &env, 1_000_000),
            None
        );
        // Completions wire → None.
        assert_eq!(
            resolve_auto_compact_window(WireApi::Completions, &env, 1_000_000),
            None
        );
        // Empty env on non-Anthropic → None, no warn.
        assert_eq!(
            resolve_auto_compact_window(WireApi::Responses, &BTreeMap::new(), 1_000_000),
            None
        );
    }

    #[test]
    fn resolve_auto_compact_window_warns_on_sanity_violations() {
        use std::collections::BTreeMap;
        let mut env = BTreeMap::new();
        // window >= max_token_count → accepted but warned (dead override).
        env.insert(
            CLAUDE_CODE_AUTO_COMPACT_WINDOW_ENV.to_string(),
            "500000".to_string(),
        );
        assert_eq!(
            resolve_auto_compact_window(WireApi::Anthropic, &env, 200_000),
            Some(500_000)
        );
        // window < MIN_COMPACTION_CONTEXT_WINDOW → accepted but warned.
        env.insert(
            CLAUDE_CODE_AUTO_COMPACT_WINDOW_ENV.to_string(),
            "50000".to_string(),
        );
        assert_eq!(
            resolve_auto_compact_window(WireApi::Anthropic, &env, 200_000),
            Some(50_000)
        );
        // window == max_token_count → warned (boundary).
        env.insert(
            CLAUDE_CODE_AUTO_COMPACT_WINDOW_ENV.to_string(),
            "200000".to_string(),
        );
        assert_eq!(
            resolve_auto_compact_window(WireApi::Anthropic, &env, 200_000),
            Some(200_000)
        );
    }
}
