//! Native provider registration: reads the cx-style provider config
//! (`~/.manox/cx.providers.config.yaml` — this project's native
//! provider format) and registers every provider × wire_api endpoint into
//! the pi kernel's [`pi::ProviderRegistry`].
//!
//! Extension autonomy (the TS `cx-bridge.ts` pattern): this module owns
//! its own config schema and credential resolution and hands the kernel
//! only declarative `ProviderConfig`s — it never depends on the host's
//! provider stack. One deliberate deviation from the TS extension: the
//! yaml's per-model `context`/`max_tokens` fields are honored (TS ignores
//! them), with precedence yaml > `[Nm]` suffix > KNOWN_MODELS > defaults.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use pi::provider_registry::{Api, Cost, InputModality, ProviderConfig, ProviderModelConfig};
use serde::Deserialize;

const CONFIG_FILE_NAME: &str = "cx.providers.config.yaml";

/// The default provider config path: `$HOME/.manox/cx.providers.config.yaml`.
pub fn default_config_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".manox")
        .join(CONFIG_FILE_NAME)
}

/// Registration name for one provider endpoint: `"{cx_name}-{wire_api}"`
/// (avoids collisions with built-in providers and between wire apis of the
/// same cx provider).
pub fn provider_registration_name(cx_name: &str, wire_api: &str) -> String {
    format!("{cx_name}-{wire_api}")
}

// ── config schema (lenient: everything optional, unknown fields ignored) ──

#[derive(Debug, Deserialize)]
struct CxConfig {
    #[serde(default)]
    providers: Vec<CxProvider>,
    /// Parsed for the visible-agents computation; not registered anywhere.
    #[serde(default)]
    agents: Vec<CxAgent>,
    /// Dedicated per-subagent models: `subagent_type` → raw
    /// `provider::model::effort` spec. Consumed by the host's subagent
    /// dispatch, not by provider registration.
    #[serde(default)]
    subagents: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct CxProvider {
    name: String,
    #[serde(default)]
    apikey_source: Option<String>,
    #[serde(default)]
    endpoints: BTreeMap<String, EndpointSpec>,
    #[serde(default)]
    models: BTreeMap<String, Option<CxModel>>,
    /// Process env for cx agents — parsed but intentionally not applied to
    /// pi registrations (parity with the TS extension).
    #[serde(default)]
    #[allow(dead_code)]
    env: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum EndpointSpec {
    Url(String),
    Detail(EndpointDetail),
}

#[derive(Debug, Deserialize)]
struct EndpointDetail {
    url: String,
    #[serde(default)]
    agents: Vec<String>,
    #[serde(default)]
    #[allow(dead_code)] // parsed for schema completeness; bearer_token is
    // covered by the registration's auth_header.
    copilot_auth: Option<String>,
}

impl EndpointSpec {
    fn url(&self) -> &str {
        match self {
            EndpointSpec::Url(url) => url,
            EndpointSpec::Detail(detail) => &detail.url,
        }
    }

    fn agents(&self) -> &[String] {
        match self {
            EndpointSpec::Url(_) => &[],
            EndpointSpec::Detail(detail) => &detail.agents,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct CxModel {
    /// Free-form description from the yaml; intentionally not surfaced in
    /// pickers (ids are the identifiers), kept for schema completeness.
    #[serde(default)]
    #[allow(dead_code)]
    desc: Option<String>,
    /// Accepted as a number or a quoted number (yaml configs use both).
    #[serde(default)]
    context: Option<serde_yaml::Value>,
    #[serde(default)]
    max_tokens: Option<serde_yaml::Value>,
    /// Empty/absent = compatible with every endpoint wire api.
    #[serde(default)]
    wire_apis: Option<Vec<String>>,
    /// Visibility allow-list for cx agents (empty = inherit).
    #[serde(default)]
    agents: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
struct CxAgent {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    binary: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    bin: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    args: Vec<String>,
    /// Ignored by resolution (hardcoded wire apis win — parity with
    /// manox-providers), parsed for schema completeness.
    #[serde(default)]
    #[allow(dead_code)]
    wire_apis: Vec<String>,
    #[serde(default)]
    #[allow(dead_code)]
    env: BTreeMap<String, String>,
}

// ── registration entry points ──

/// Register every provider endpoint from the config file into `registry`.
/// A missing file is not an error (returns 0) — a fresh install simply has
/// no providers. Synchronous on purpose: credential resolution may hit the
/// OS keychain or a shell command, matching manox-providers' blocking
/// semantics; callers run this at startup/reload.
pub fn register_providers(
    registry: &pi::ProviderRegistry,
    path: impl AsRef<Path>,
) -> anyhow::Result<usize> {
    let path = path.as_ref();
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(err) => anyhow::bail!("read {}: {err}", path.display()),
    };
    register_providers_yaml(registry, &raw)
}

/// Registration from an already-read config document (the testable core).
pub fn register_providers_yaml(
    registry: &pi::ProviderRegistry,
    raw: &str,
) -> anyhow::Result<usize> {
    let config = parse_config(raw)?;

    let mut registered = 0;
    for provider in &config.providers {
        // A credential failure skips this provider only (warn-logged, never
        // blocking the rest — parity with the manox registry build).
        let api_key = match resolve_apikey(provider.apikey_source.as_deref()) {
            Ok(key) => key,
            Err(err) => {
                tracing::warn!(provider = %provider.name, error = %err, "provider skipped: api key unresolvable");
                continue;
            }
        };
        for (wire_api, spec) in &provider.endpoints {
            let Some(api) = wire_api_to_api(wire_api) else {
                continue;
            };
            let models: Vec<ProviderModelConfig> = provider
                .models
                .iter()
                .filter(|(_, model)| {
                    model_supports_wire_api(
                        model.as_ref().and_then(|m| m.wire_apis.as_deref()),
                        wire_api,
                    )
                })
                .map(|(raw_id, model)| {
                    build_model_config(&config, raw_id, model.as_ref(), wire_api, spec)
                })
                .collect();
            if models.is_empty() {
                continue;
            }
            let name = provider_registration_name(&provider.name, wire_api);
            registry
                .register_provider(
                    &name,
                    ProviderConfig {
                        name: Some(provider.name.clone()),
                        base_url: Some(spec.url().to_string()),
                        api_key: api_key.clone(),
                        api: Some(api),
                        headers: None,
                        auth_header: true,
                        models,
                    },
                )
                .map_err(|err| anyhow::anyhow!("register provider {name:?}: {err}"))?;
            registered += 1;
        }
    }
    Ok(registered)
}

/// Parse the cx providers config document — shared by registration and the
/// subagent-model loader so one broken file cannot be seen differently by
/// the two consumers.
fn parse_config(raw: &str) -> anyhow::Result<CxConfig> {
    serde_yaml::from_str(raw).map_err(|err| anyhow::anyhow!("parse cx providers config: {err}"))
}

/// Dedicated per-subagent models from the cx providers config's top-level
/// `subagents:` map (`subagent_type: "provider::model::effort"`). A missing
/// file yields an empty map (mirrors [`register_providers`]); a parse
/// failure is a hard error the caller warns about — never silent.
pub fn load_subagent_models(path: impl AsRef<Path>) -> anyhow::Result<HashMap<String, String>> {
    let path = path.as_ref();
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(err) => anyhow::bail!("read {}: {err}", path.display()),
    };
    Ok(parse_config(&raw)?.subagents.into_iter().collect())
}

fn wire_api_to_api(wire_api: &str) -> Option<Api> {
    match wire_api {
        "anthropic" => Some(Api::AnthropicMessages),
        "completions" => Some(Api::OpenAiCompletions),
        "responses" => Some(Api::OpenAiResponses),
        _ => None,
    }
}

/// Inverse of [`wire_api_to_api`] for kernel model api strings (`Model.api`):
/// the cx wire key the wire protocol registers under. `None` for non-cx apis.
pub fn model_api_to_wire_key(api: &str) -> Option<&'static str> {
    match api {
        "anthropic" => Some("anthropic"),
        "openai_responses" => Some("responses"),
        "openai_completions" => Some("completions"),
        _ => None,
    }
}

/// cx `wire_apis` semantics: empty/absent means compatible with every
/// endpoint wire api.
fn model_supports_wire_api(wire_apis: Option<&[String]>, endpoint_wire_api: &str) -> bool {
    match wire_apis {
        None | Some([]) => true,
        Some(list) => list.iter().any(|w| w == endpoint_wire_api),
    }
}

// ── apikey resolution (mirrors manox_providers::resolve_apikey) ──

/// Resolve an `apikey_source` the way manox-providers does, with one
/// deliberate difference: `env:VAR` is **not** read eagerly — it is passed
/// through as the kernel's `$VAR` interpolation syntax so the value is
/// re-read on every request (TS pi resolves api keys uncached for the same
/// reason).
///
/// `Ok(None)` never happens for a present source; a missing source is a
/// hard error (parity with manox-providers, which refuses to build a model
/// without one).
pub fn resolve_apikey(source: Option<&str>) -> Result<Option<String>, String> {
    let Some(source) = source else {
        return Err("no apikey_source configured".to_string());
    };
    if let Some(service) = source.strip_prefix("keychain:") {
        return keychain_secret(service).map(Some);
    }
    if let Some(var) = source.strip_prefix("env:") {
        // The kernel interpolation stops at the first non-identifier char,
        // so an invalid name would resolve silently to the wrong variable;
        // fail the provider instead (POSIX names: [_A-Za-z][_A-Za-z0-9]*).
        let valid = !var.is_empty()
            && var
                .chars()
                .enumerate()
                .all(|(i, c)| c == '_' || c.is_ascii_alphabetic() || (i > 0 && c.is_ascii_digit()));
        if !valid {
            return Err(format!("invalid environment variable name in `env:{var}`"));
        }
        return Ok(Some(format!("${var}")));
    }
    if let Some(literal) = source.strip_prefix("literal:") {
        return Ok(Some(literal.to_string()));
    }
    if let Some(cmd) = source.strip_prefix("$(") {
        let cmd = cmd.trim_end_matches(')');
        return run_shell_once(cmd).map(Some);
    }
    Err(format!("unsupported apikey_source format: `{source}`"))
}

/// Read a generic password from the macOS Keychain (one-shot; the
/// registration snapshot carries the literal afterwards — no per-request
/// `security` calls).
fn keychain_secret(service: &str) -> Result<String, String> {
    if !cfg!(target_os = "macos") {
        return Err(format!(
            "`keychain:` is only supported on macOS Keychain; use `env:` for {service}"
        ));
    }
    let user = std::env::var("USER").unwrap_or_default();
    let output = std::process::Command::new("security")
        .args(["find-generic-password", "-a", &user, "-s", service, "-w"])
        .output()
        .map_err(|err| format!("invoke macOS Keychain (service={service}): {err}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(format!(
            "read {service:?} from Keychain: {}",
            if stderr.is_empty() {
                "unknown error".to_string()
            } else {
                stderr
            }
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .trim_end_matches(['\n', '\r'])
        .to_string())
}

/// Execute a `$(command)` apikey source once and capture stdout.
fn run_shell_once(cmd: &str) -> Result<String, String> {
    let output = std::process::Command::new("sh")
        .args(["-c", cmd])
        .output()
        .map_err(|err| format!("run shell command `{cmd}`: {err}"))?;
    if !output.status.success() {
        return Err(format!("shell command `{cmd}` failed"));
    }
    String::from_utf8(output.stdout)
        .map(|stdout| stdout.trim().to_string())
        .map_err(|err| format!("shell command `{cmd}` output: {err}"))
}

// ── model metadata ──

/// Known-model metadata lookup — cx yaml does not carry contextWindow /
/// maxTokens / reasoning / cost, so the extension supplies them (table
/// ported from the TS cx-bridge extension).
struct KnownModelMeta {
    context_window: u64,
    max_tokens: u64,
    reasoning: bool,
    cost: Option<Cost>,
    input: Option<Vec<InputModality>>,
}

const fn cost(input: f64, output: f64, cache_read: f64, cache_write: f64) -> Cost {
    Cost {
        input,
        output,
        cache_read,
        cache_write,
    }
}

fn known_model_meta(id: &str) -> KnownModelMeta {
    let claude_flagship = KnownModelMeta {
        context_window: 200_000,
        max_tokens: 32_000,
        reasoning: true,
        cost: Some(cost(15.0, 75.0, 1.5, 18.75)),
        input: None,
    };
    match id {
        "claude-opus-4-8" | "claude-opus-4-7" | "claude-opus-4-6" | "claude-fable-5" => {
            claude_flagship
        }
        "claude-sonnet-4-6" => KnownModelMeta {
            context_window: 200_000,
            max_tokens: 16_384,
            reasoning: true,
            cost: Some(cost(3.0, 15.0, 0.3, 3.75)),
            input: None,
        },
        "claude-haiku-4-5-20251001" => KnownModelMeta {
            context_window: 200_000,
            max_tokens: 8_192,
            reasoning: true,
            cost: Some(cost(0.8, 4.0, 0.08, 1.0)),
            input: None,
        },
        "qwen3.7-max" => KnownModelMeta {
            context_window: 131_072,
            max_tokens: 8_192,
            reasoning: false,
            cost: None,
            input: Some(vec![InputModality::Text, InputModality::Image]),
        },
        "qwen3.7-plus"
        | "deepseek-v4.1"
        | "deepseek-v4-pro"
        | "glm-5.2"
        | "glm-5.1"
        | "MiniMax/MiniMax-M2.7"
        | "kimi-k2.7-code"
        | "mimo-v2-pro"
        | "mimo-v2.5-pro" => KnownModelMeta {
            context_window: 131_072,
            max_tokens: 8_192,
            reasoning: false,
            cost: None,
            input: None,
        },
        _ => KnownModelMeta {
            context_window: 131_072,
            max_tokens: 8_192,
            reasoning: false,
            cost: None,
            input: Some(vec![InputModality::Text, InputModality::Image]),
        },
    }
}

/// Split a raw model id into the wire-facing id and an optional trailing
/// context-window hint (`deepseek-v4-flash[1m]` → `("deepseek-v4-flash",
/// Some(1_000_000))`). Grammar mirrors `manox_providers::parse_context_window`
/// (`[200k]`, `[1m123k]`, bare numbers, k/m units); an unparseable or
/// non-trailing group leaves the id intact (sent to the API verbatim).
pub fn parse_model_id(raw: &str) -> (String, Option<u64>) {
    let Some(open) = trailing_bracket_open(raw) else {
        return (raw.to_string(), None);
    };
    let inner = &raw[open + 1..raw.len() - 1];
    match parse_context_window(inner) {
        Some(window) => (raw[..open].to_string(), Some(window)),
        None => (raw.to_string(), None),
    }
}

/// Byte index of the `[` opening a single trailing `[...]` group, or
/// `None` when the id has no trailing group or multiple brackets.
fn trailing_bracket_open(id: &str) -> Option<usize> {
    if !id.ends_with(']') {
        return None;
    }
    let open = id.rfind('[')?;
    let prefix = &id[..open];
    if prefix.contains('[') || prefix.contains(']') {
        return None;
    }
    Some(open)
}

/// Parse a context-window suffix term list: digit runs with optional
/// `k`/`m` unit letters, additive (`1m123k` = 1_123_000). Port of
/// `manox_providers::parse_context_window` (kept in sync by tests; the
/// extension cannot depend on manox-providers by design).
fn parse_context_window(s: &str) -> Option<u64> {
    let t = s.trim();
    if t.is_empty() || t.contains(char::is_whitespace) {
        return None;
    }
    let b = t.as_bytes();
    let n = b.len();
    let mut i = 0;
    let mut total: u64 = 0;
    let mut saw_term = false;
    while i < n {
        if !b[i].is_ascii_digit() {
            return None;
        }
        let mut j = i;
        while j < n && b[j].is_ascii_digit() {
            j += 1;
        }
        let digits = &b[i..j];
        if digits.len() > 1 && digits[0] == b'0' {
            return None;
        }
        i = j;
        let mult: u64 = if i < n {
            match b[i] {
                b'k' | b'K' => {
                    i += 1;
                    1_000
                }
                b'm' | b'M' => {
                    i += 1;
                    1_000_000
                }
                _ => 1,
            }
        } else {
            1
        };
        let val: u64 = std::str::from_utf8(digits).ok()?.parse().ok()?;
        total = total.checked_add(val.checked_mul(mult)?)?;
        saw_term = true;
    }
    saw_term.then_some(total)
}

/// Accept a yaml number or a quoted number.
fn value_as_u64(value: &serde_yaml::Value) -> Option<u64> {
    match value {
        serde_yaml::Value::Number(n) => n.as_u64(),
        serde_yaml::Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Build one kernel model config. Context/max-tokens precedence (explicit
/// deviation from the TS extension, which drops the yaml fields): yaml
/// field > `[Nm]` suffix hint > KNOWN_MODELS > defaults.
fn build_model_config(
    config: &CxConfig,
    raw_id: &str,
    cx_model: Option<&CxModel>,
    wire_api: &str,
    endpoint: &EndpointSpec,
) -> ProviderModelConfig {
    let (id, suffix_hint) = parse_model_id(raw_id);
    let known = known_model_meta(&id);
    let yaml_context = cx_model
        .and_then(|m| m.context.as_ref())
        .and_then(value_as_u64);
    let yaml_max_tokens = cx_model
        .and_then(|m| m.max_tokens.as_ref())
        .and_then(value_as_u64);
    let context_window = yaml_context.or(suffix_hint).unwrap_or(known.context_window);
    let max_tokens = yaml_max_tokens.unwrap_or(known.max_tokens);
    // Display name is the wire id (users expect ids in the picker; the
    // yaml `desc` is free-form marketing copy, not an identifier).
    let model_agents = cx_model.map(|m| m.agents.clone()).unwrap_or_default();
    ProviderModelConfig {
        name: id.clone(),
        id,
        reasoning: known.reasoning,
        input: known
            .input
            .unwrap_or_else(|| vec![InputModality::Text, InputModality::Image]),
        context_window,
        max_tokens,
        cost: known.cost.unwrap_or_default(),
        api: None,
        base_url: None,
        metadata: {
            let mut meta = std::collections::HashMap::new();
            meta.insert(
                "agents".to_string(),
                serde_json::json!(effective_agents(
                    config,
                    wire_api,
                    endpoint.agents(),
                    &model_agents
                )),
            );
            meta.insert("config_id".to_string(), serde_json::json!(raw_id));
            meta
        },
    }
}

// ── visible agents (port of manox_providers::effective_agents_for_model) ──

/// Canonicalize a config agent id (legacy `codex-app` → `codex`,
/// `Codex.app` → `ChatGPT.app`, `codex+` → `codex` — parity with
/// manox-providers).
fn canonical_agent_id(agent_id: &str) -> &str {
    match agent_id {
        "codex-app" => "codex",
        "Codex.app" => "ChatGPT.app",
        "codex+" => "codex",
        _ => agent_id,
    }
}

/// Canonicalize + dedupe preserving first-seen order.
fn normalize_agent_ids(agent_ids: &[String]) -> Vec<String> {
    let mut normalized: Vec<String> = Vec::new();
    for agent_id in agent_ids {
        let canonical = canonical_agent_id(agent_id).to_string();
        if !normalized.contains(&canonical) {
            normalized.push(canonical);
        }
    }
    normalized
}

/// Hardcoded wire apis each built-in agent supports — the config file's
/// per-agent `wire_apis` is ignored (parity with manox-providers).
fn hardcoded_wire_apis(agent_id: &str) -> &'static [&'static str] {
    match canonical_agent_id(agent_id) {
        "copilot" => &["anthropic", "responses", "completions"],
        "claude" | "VS Code" => &["anthropic"],
        "codex" | "ChatGPT.app" => &["responses"],
        _ => &[],
    }
}

/// The resolved agent id universe: user agents (canonical, deduped, `codex`
/// expanded into `codex` + `ChatGPT.app`, `claude` into `claude` + `VS Code`).
fn resolved_agent_ids(config: &CxConfig) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    for agent in &config.agents {
        let Some(raw_id) = &agent.id else { continue };
        let id = canonical_agent_id(raw_id).to_string();
        if ids.contains(&id) {
            continue;
        }
        if id == "codex" {
            ids.push("codex".to_string());
            ids.push("ChatGPT.app".to_string());
        } else if id == "claude" {
            // claude expands into a CLI entry and a VS Code desktop entry
            // (injection-type agent, parity with manox-providers).
            ids.push("claude".to_string());
            ids.push("VS Code".to_string());
        } else {
            ids.push(id);
        }
    }
    ids
}

/// The effective agent visibility for one model on one endpoint — the port
/// of `manox_providers::effective_agents_for_model`: start from the agents
/// whose hardcoded wire apis match the endpoint's wire api, then apply the
/// endpoint's and the model's `agents` lists as allow-list filters (an
/// empty filter is a no-op).
fn effective_agents(
    config: &CxConfig,
    wire_api: &str,
    endpoint_agents: &[String],
    model_agents: &[String],
) -> Vec<String> {
    let mut resolved: Vec<String> = resolved_agent_ids(config)
        .into_iter()
        .filter(|id| hardcoded_wire_apis(id).contains(&wire_api))
        .collect();
    for filter in [endpoint_agents, model_agents] {
        if filter.is_empty() {
            continue;
        }
        let allowed = normalize_agent_ids(filter);
        resolved.retain(|id| allowed.iter().any(|allowed_id| allowed_id == id));
    }
    resolved
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixture shaped like the live config: two providers sharing a model
    /// id, `[1m]` suffixes, an empty `{}` model entry, wire_apis
    /// filtering, and a yaml `context`/`max_tokens` override.
    const FIXTURE: &str = r#"
providers:
- name: DeepSeek
  apikey_source: literal:sk-test
  endpoints:
    anthropic:
      url: https://api.deepseek.com/anthropic
      agents: []
      copilot_auth: null
    responses: https://api.deepseek.com
  models:
    deepseek-v4-pro[1m]:
      wire_apis: [anthropic, responses, completions]
    deepseek-v4-flash[1m]:
      wire_apis: [anthropic]
- name: Token Plan
  apikey_source: env:TP_KEY
  endpoints:
    anthropic:
      url: https://tp.example.com/apps/anthropic
    completions:
      url: https://tp.example.com/compatible-mode/v1
  models:
    deepseek-v4-pro[1m]:
      context: 1000000
      max_tokens: 131072
      wire_apis: [anthropic]
    glm-5.2[1m]:
      context: 1000000
      wire_apis: [anthropic]
    qwen-empty[1m]: {}
agents:
- id: claude
  binary: claude
- id: codex
  binary: codex
"#;

    fn register_fixture() -> pi::ProviderRegistry {
        let registry = pi::ProviderRegistry::new();
        let count = register_providers_yaml(&registry, FIXTURE).unwrap();
        assert_eq!(count, 4);
        registry
    }

    #[test]
    fn registers_every_endpoint_with_suffixed_names() {
        let registry = register_fixture();
        assert_eq!(
            registry.provider_names(),
            vec![
                "DeepSeek-anthropic",
                "DeepSeek-responses",
                "Token Plan-anthropic",
                "Token Plan-completions",
            ]
        );
    }

    #[test]
    fn strips_context_suffix_and_filters_wire_apis() {
        let registry = register_fixture();
        let anthropic = registry.resolve_model("DeepSeek-anthropic", "deepseek-v4-flash");
        assert!(anthropic.is_some(), "[1m] suffix must be stripped");
        assert!(
            registry
                .resolve_model("DeepSeek-responses", "deepseek-v4-flash")
                .is_none(),
            "flash only lists anthropic in wire_apis"
        );
        assert!(
            registry
                .resolve_model("DeepSeek-responses", "deepseek-v4-pro")
                .is_some()
        );
    }

    #[test]
    fn duplicate_model_ids_across_providers_coexist() {
        let registry = register_fixture();
        assert!(
            registry
                .resolve_model("DeepSeek-anthropic", "deepseek-v4-pro")
                .is_some()
        );
        assert!(
            registry
                .resolve_model("Token Plan-anthropic", "deepseek-v4-pro")
                .is_some()
        );
    }

    #[test]
    fn yaml_context_and_max_tokens_win_over_known_table() {
        let registry = register_fixture();
        let model = registry
            .resolve_model("Token Plan-anthropic", "deepseek-v4-pro")
            .unwrap();
        assert_eq!(model.context_window, 1_000_000);
        assert_eq!(model.max_tokens, 131_072);
        let glm = registry
            .resolve_model("Token Plan-anthropic", "glm-5.2")
            .unwrap();
        assert_eq!(glm.context_window, 1_000_000);
        // KNOWN_MODELS default for glm-5.2 max_tokens applies.
        assert_eq!(glm.max_tokens, 8_192);
    }

    #[test]
    fn suffix_hint_beats_known_table() {
        let registry = register_fixture();
        let model = registry
            .resolve_model("DeepSeek-anthropic", "deepseek-v4-pro")
            .unwrap();
        // No yaml context → [1m] hint wins over KNOWN_MODELS' 131072.
        assert_eq!(model.context_window, 1_000_000);
    }

    #[test]
    fn empty_model_entry_gets_defaults_with_suffix_hint() {
        let registry = register_fixture();
        let model = registry
            .resolve_model("Token Plan-anthropic", "qwen-empty")
            .unwrap();
        assert_eq!(model.context_window, 1_000_000);
        assert_eq!(model.max_tokens, 8_192);
        assert_eq!(model.metadata.get("name").unwrap(), "qwen-empty");
    }

    #[test]
    fn apikey_env_source_passes_through_as_kernel_interpolation() {
        let registry = register_fixture();
        let config = registry.provider_config("Token Plan-anthropic").unwrap();
        assert_eq!(config.api_key.as_deref(), Some("$TP_KEY"));
        let config = registry.provider_config("DeepSeek-anthropic").unwrap();
        assert_eq!(config.api_key.as_deref(), Some("sk-test"));
        assert!(config.auth_header);
    }

    #[test]
    fn resolve_apikey_shapes() {
        assert_eq!(
            resolve_apikey(Some("literal:abc")).unwrap().as_deref(),
            Some("abc")
        );
        assert_eq!(
            resolve_apikey(Some("env:MY_KEY")).unwrap().as_deref(),
            Some("$MY_KEY")
        );
        assert_eq!(
            resolve_apikey(Some("$(echo hi)")).unwrap().as_deref(),
            Some("hi")
        );
        assert!(resolve_apikey(Some("bogus-format")).is_err());
        assert!(resolve_apikey(None).is_err());
    }

    #[test]
    fn unknown_wire_api_endpoints_are_skipped() {
        let registry = pi::ProviderRegistry::new();
        let yaml = r#"
providers:
- name: X
  apikey_source: literal:k
  endpoints:
    grpc: https://x.example
  models:
    m: {}
"#;
        assert_eq!(register_providers_yaml(&registry, yaml).unwrap(), 0);
        assert!(registry.provider_names().is_empty());
    }

    #[test]
    fn missing_file_registers_nothing() {
        let registry = pi::ProviderRegistry::new();
        let count = register_providers(&registry, "/nonexistent/cx.providers.config.yaml").unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn unresolvable_apikey_skips_only_that_provider() {
        let registry = pi::ProviderRegistry::new();
        let yaml = r#"
providers:
- name: Broken
  apikey_source: bogus
  endpoints:
    anthropic: https://broken.example
  models:
    m: {}
- name: Fine
  apikey_source: literal:k
  endpoints:
    anthropic: https://fine.example
  models:
    m: {}
"#;
        assert_eq!(register_providers_yaml(&registry, yaml).unwrap(), 1);
        assert_eq!(registry.provider_names(), vec!["Fine-anthropic"]);
    }

    #[test]
    fn parse_model_id_shapes() {
        assert_eq!(parse_model_id("m[1m]"), ("m".into(), Some(1_000_000)));
        assert_eq!(parse_model_id("m[2m]"), ("m".into(), Some(2_000_000)));
        assert_eq!(parse_model_id("m"), ("m".into(), None));
        assert_eq!(parse_model_id("m[1x]"), ("m[1x]".into(), None));
        assert_eq!(parse_model_id("m[]"), ("m[]".into(), None));
        // cx suffix grammar parity: k units, additive terms, bare numbers,
        // uppercase units; multi-group ids stay intact.
        assert_eq!(
            parse_model_id("glm-5.2[200k]"),
            ("glm-5.2".into(), Some(200_000))
        );
        assert_eq!(parse_model_id("m[1m123k]"), ("m".into(), Some(1_123_000)));
        assert_eq!(parse_model_id("m[65536]"), ("m".into(), Some(65_536)));
        assert_eq!(parse_model_id("m[2M]"), ("m".into(), Some(2_000_000)));
        assert_eq!(parse_model_id("m[1m][2m]"), ("m[1m][2m]".into(), None));
        assert_eq!(parse_model_id("m[01m]"), ("m[01m]".into(), None));
    }

    #[test]
    fn env_source_rejects_invalid_variable_names() {
        assert_eq!(
            resolve_apikey(Some("env:GOOD_NAME1")).unwrap().as_deref(),
            Some("$GOOD_NAME1")
        );
        assert!(resolve_apikey(Some("env:MY-VAR")).is_err());
        assert!(resolve_apikey(Some("env:1LEAD")).is_err());
        assert!(resolve_apikey(Some("env:")).is_err());
    }

    #[test]
    fn effective_agents_parity_with_manox_providers_rules() {
        let config: CxConfig = serde_yaml::from_str(FIXTURE).unwrap();
        // anthropic endpoint, no filters: claude + VS Code
        // (codex/ChatGPT.app are responses-only; copilot unconfigured).
        assert_eq!(
            effective_agents(&config, "anthropic", &[], &[]),
            vec!["claude", "VS Code"]
        );
        // responses endpoint: codex expands to codex + ChatGPT.app.
        assert_eq!(
            effective_agents(&config, "responses", &[], &[]),
            vec!["codex", "ChatGPT.app"]
        );
        // Model allow-list narrows.
        assert_eq!(
            effective_agents(&config, "anthropic", &[], &["claude".into()]),
            vec!["claude"]
        );
        // Endpoint allow-list applies first, then model.
        assert_eq!(
            effective_agents(
                &config,
                "anthropic",
                &["claude".into(), "VS Code".into()],
                &["VS Code".into()]
            ),
            vec!["VS Code"]
        );
        // Allow-list naming an absent agent yields empty.
        assert_eq!(
            effective_agents(&config, "anthropic", &[], &["copilot".into()]),
            Vec::<String>::new()
        );
    }

    #[test]
    fn codex_plus_legacy_id_canonicalizes_to_codex() {
        assert_eq!(canonical_agent_id("codex+"), "codex");
        let yaml = r#"
agents:
- id: codex
  binary: codex
- id: codex+
  binary: codex+
"#;
        let config: CxConfig = serde_yaml::from_str(yaml).unwrap();
        // The stale legacy entry dedupes onto codex (plus its ChatGPT.app expansion).
        assert_eq!(resolved_agent_ids(&config), vec!["codex", "ChatGPT.app"]);
        // Filters written with the legacy id match the canonical agent.
        assert_eq!(
            effective_agents(&config, "responses", &[], &["codex+".into()]),
            vec!["codex"]
        );
    }

    /// Schema smoke test against the live user config (not CI-portable).
    /// Registration count depends on keychain reachability, so the hard
    /// assertion is that the live document parses with this schema; any
    /// registered model must have its `[Nm]` suffix stripped.
    #[test]
    #[ignore = "reads the local user config"]
    fn live_config_parses_and_registers() {
        let raw = std::fs::read_to_string(default_config_path()).unwrap();
        let parsed: Result<CxConfig, _> = serde_yaml::from_str(&raw);
        assert!(parsed.is_ok(), "live config must parse: {parsed:?}");
        let registry = pi::ProviderRegistry::new();
        let count = register_providers_yaml(&registry, &raw).unwrap();
        for model in registry.models() {
            assert!(!model.id.contains('['), "suffix must be stripped");
        }
        eprintln!("live config registered {count} provider endpoints");
    }

    #[test]
    fn model_metadata_carries_agents() {
        let registry = register_fixture();
        let model = registry
            .resolve_model("DeepSeek-anthropic", "deepseek-v4-flash")
            .unwrap();
        assert_eq!(
            model.metadata.get("agents").unwrap(),
            &serde_json::json!(["claude", "VS Code"])
        );
        assert_eq!(
            model.metadata.get("provider_display_name").unwrap(),
            "DeepSeek"
        );
    }

    #[test]
    fn model_api_to_wire_key_inverts_wire_api_to_api() {
        assert_eq!(model_api_to_wire_key("anthropic"), Some("anthropic"));
        assert_eq!(model_api_to_wire_key("openai_responses"), Some("responses"));
        assert_eq!(
            model_api_to_wire_key("openai_completions"),
            Some("completions")
        );
        assert_eq!(model_api_to_wire_key("anthropic-messages"), None);
    }

    #[test]
    fn load_subagent_models_reads_the_subagents_map() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cx.providers.config.yaml");
        // A missing file yields an empty map, like `register_providers`.
        assert!(load_subagent_models(&path).unwrap().is_empty());
        // A config without the `subagents:` key yields an empty map.
        std::fs::write(&path, FIXTURE).unwrap();
        assert!(load_subagent_models(&path).unwrap().is_empty());
        // The `subagents:` block returns its raw spec strings verbatim.
        let with_subagents = format!(
            "{FIXTURE}\nsubagents:\n  Explore: 百炼::glm-5.3::high\n  Sailor: DeepSeek::deepseek-v4-flash::high\n"
        );
        std::fs::write(&path, &with_subagents).unwrap();
        let map = load_subagent_models(&path).unwrap();
        assert_eq!(map.get("Explore").unwrap(), "百炼::glm-5.3::high");
        assert_eq!(
            map.get("Sailor").unwrap(),
            "DeepSeek::deepseek-v4-flash::high"
        );
        assert_eq!(map.len(), 2);
        // A broken document is a hard error, never a silent empty map.
        std::fs::write(&path, "providers: [unclosed").unwrap();
        let err = load_subagent_models(&path).unwrap_err();
        assert!(
            err.to_string().contains("parse cx providers config"),
            "{err}"
        );
    }

    #[test]
    fn subagents_block_does_not_affect_registration() {
        let registry = pi::ProviderRegistry::new();
        let with_subagents = format!("{FIXTURE}\nsubagents:\n  Explore: P::m::high\n");
        let count = register_providers_yaml(&registry, &with_subagents).unwrap();
        assert_eq!(
            count, 4,
            "the `subagents` key must not disturb registration"
        );
        assert_eq!(registry.provider_names().len(), 4);
    }
}
