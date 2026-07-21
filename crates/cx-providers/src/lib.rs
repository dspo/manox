//! cx providers config parsing — single source of truth shared by `cx` and `manox`.
//!
//! Config file: `~/.config/cx/cx.providers.config.yaml`. Schema:
//! - `providers: Vec<ProviderConfig>`, each with `name` / `apikey_source` /
//!   `models: BTreeMap<id, ProviderModelConfig>` / `endpoints: BTreeMap<wire_api, spec>` / `env`.
//! - `agents: Vec<AgentConfig>`, each describing an external agent binary cx can launch.
//! - Each model supports several `wire_apis` (anthropic / responses / completions).
//! - `ResolvedModel` is a fully resolved, callable model (provider + endpoint + wire_api + auth).
//!
//! `apikey_source` resolves via `resolve_apikey` (`keychain:SERVICE` / `env:VAR` / `literal:` /
//! `$(shell ...)`). Keychain uses the macOS `security` CLI — no keyring crate, zero extra deps.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub const PROVIDER_CONFIG_FILE_NAME: &str = "cx.providers.config.yaml";
pub const LEGACY_PROVIDER_CONFIG_FILE_NAME: &str = "config.yaml";

// ═══════════════════════════════════════════════════
// Wire protocol + auth strategy
// ═══════════════════════════════════════════════════

/// Wire protocol. `priority` picks the default when a model exposes multiple wires (anthropic wins).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum WireApi {
    Responses,
    Completions,
    Anthropic,
    Unavailable,
}

impl WireApi {
    /// Infallible parse: unknown strings fall back to `Unavailable`.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s {
            "responses" => Self::Responses,
            "completions" => Self::Completions,
            "anthropic" => Self::Anthropic,
            _ => Self::Unavailable,
        }
    }

    pub fn display(self) -> &'static str {
        match self {
            Self::Responses => "responses",
            Self::Completions => "completions",
            Self::Anthropic => "anthropic",
            Self::Unavailable => "unavailable",
        }
    }

    /// anthropic(0) > responses(1) > completions(2) > unavailable(3).
    pub fn priority(self) -> u8 {
        match self {
            Self::Anthropic => 0,
            Self::Responses => 1,
            Self::Completions => 2,
            Self::Unavailable => 3,
        }
    }
}

/// Auth-header strategy for copilot-style providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopilotAuth {
    ApiKey,
    BearerToken,
}

impl CopilotAuth {
    pub fn from_endpoint(endpoint: &EndpointConfig) -> Self {
        match endpoint.copilot_auth.as_deref() {
            Some("bearer_token") => Self::BearerToken,
            _ => Self::ApiKey,
        }
    }
}

/// How a provider's API key is materialized, for the TUI add-provider flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiKeySourceKind {
    None,
    Env,
    Keychain,
    Literal,
    Shell,
}

impl ApiKeySourceKind {
    pub fn all() -> [Self; 5] {
        [
            Self::None,
            Self::Env,
            Self::Keychain,
            Self::Literal,
            Self::Shell,
        ]
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::None => "不设置",
            Self::Env => "env:VAR",
            Self::Keychain => "keychain:SERVICE",
            Self::Literal => "literal:value",
            Self::Shell => "$(shell command)",
        }
    }

    pub fn prompt(self) -> &'static str {
        match self {
            Self::None => "不设置 apikey_source，保留为空",
            Self::Env => "输入环境变量名，例如 DASHSCOPE_API_KEY",
            Self::Keychain => "输入 Keychain service 名称，例如 DASHSCOPE_API_KEY",
            Self::Literal => "输入固定值，仅建议本地调试使用",
            Self::Shell => "输入 shell 命令内容，不要带 $( )，例如 op read ...",
        }
    }

    pub fn build(self, value: &str) -> Option<String> {
        let value = value.trim();
        match self {
            Self::None => None,
            Self::Env => Some(format!("env:{value}")),
            Self::Keychain => Some(format!("keychain:{value}")),
            Self::Literal => Some(format!("literal:{value}")),
            Self::Shell => Some(format!("$({value})")),
        }
    }
}

// ═══════════════════════════════════════════════════
// Deserialization structs (mirror the YAML schema)
// ═══════════════════════════════════════════════════

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct CxConfig {
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
    #[serde(default)]
    pub agents: Vec<AgentConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProviderConfig {
    pub name: String,
    #[serde(default)]
    pub apikey_source: Option<String>,
    #[serde(default)]
    pub models: BTreeMap<String, ProviderModelConfig>,
    #[serde(default)]
    pub endpoints: BTreeMap<String, ProviderEndpointSpec>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum ProviderEndpointSpec {
    Url(String),
    Detailed(ProviderEndpointDetail),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProviderEndpointDetail {
    pub url: String,
    #[serde(default)]
    pub agents: Vec<String>,
    #[serde(default)]
    pub copilot_auth: Option<String>,
}

/// A normalized endpoint (wire_api → url + agents + copilot_auth + the models it serves).
#[derive(Debug, Clone)]
pub struct EndpointConfig {
    pub wire_api: String,
    pub url: String,
    pub agents: Vec<String>,
    pub copilot_auth: Option<String>,
    pub models: Vec<ModelConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ModelConfig {
    pub id: String,
    #[serde(default)]
    pub desc: Option<String>,
    #[serde(default)]
    pub wire_apis: Vec<String>,
    #[serde(default)]
    pub agents: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Maximum tokens a single response may emit. When unset, the consumer
    /// derives a heuristic from the context window.
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
    /// Model context window size in tokens. Numeric counterpart to the display
    /// `context` string; takes precedence for request sizing when present.
    #[serde(default)]
    pub max_tokens: Option<u64>,
    /// Whether the model accepts tool definitions. Defaults to `true` at
    /// resolution time so existing reasoning/chat models keep tool access.
    #[serde(default)]
    pub supports_tools: Option<bool>,
    /// Whether the model can ingest image content. Defaults to `false` at
    /// resolution time so models must opt in to vision.
    #[serde(default)]
    pub supports_images: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct ProviderModelConfig {
    #[serde(default)]
    pub desc: Option<String>,
    #[serde(default)]
    pub wire_apis: Vec<String>,
    #[serde(default)]
    pub agents: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
    #[serde(default)]
    pub max_tokens: Option<u64>,
    #[serde(default)]
    pub supports_tools: Option<bool>,
    #[serde(default)]
    pub supports_images: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AgentConfig {
    pub id: String,
    #[serde(alias = "bin")]
    pub binary: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub wire_apis: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

// ═══════════════════════════════════════════════════
// Normalization + resolution
// ═══════════════════════════════════════════════════

impl ProviderConfig {
    /// Cross-normalize `endpoints` against `models`: each endpoint carries the models it supports.
    pub fn normalized_endpoints(&self) -> Vec<EndpointConfig> {
        let mut endpoints = self
            .endpoints
            .iter()
            .map(|(wire_api, spec)| {
                let (url, agents, copilot_auth) = match spec {
                    ProviderEndpointSpec::Url(url) => (url.clone(), Vec::new(), None),
                    ProviderEndpointSpec::Detailed(detail) => (
                        detail.url.clone(),
                        detail.agents.clone(),
                        detail.copilot_auth.clone(),
                    ),
                };
                let models = self
                    .models
                    .iter()
                    .filter(|(_, model)| {
                        model.wire_apis.is_empty()
                            || model.wire_apis.iter().any(|candidate| {
                                WireApi::from_str(candidate) == WireApi::from_str(wire_api)
                            })
                    })
                    .map(|(id, model)| ModelConfig {
                        id: id.clone(),
                        desc: model.desc.clone(),
                        wire_apis: model.wire_apis.clone(),
                        agents: model.agents.clone(),
                        env: model.env.clone(),
                        max_output_tokens: model.max_output_tokens,
                        max_tokens: model.max_tokens,
                        supports_tools: model.supports_tools,
                        supports_images: model.supports_images,
                    })
                    .collect();

                EndpointConfig {
                    wire_api: wire_api.clone(),
                    url,
                    agents,
                    copilot_auth,
                    models,
                }
            })
            .collect::<Vec<_>>();
        endpoints.sort_by_key(|endpoint| WireApi::from_str(&endpoint.wire_api).priority());
        endpoints
    }

    pub fn has_endpoints(&self) -> bool {
        !self.normalized_endpoints().is_empty()
    }
}

/// A fully resolved, callable model.
#[derive(Debug, Clone)]
pub struct ResolvedModel {
    pub id: String,
    pub desc: String,
    pub wire_api: WireApi,
    pub model_wire_apis: Vec<WireApi>,
    pub provider_name: String,
    pub endpoint_url: String,
    pub visible_agents: Vec<String>,
    pub copilot_auth: CopilotAuth,
    pub env: BTreeMap<String, String>,
    /// apikey resolution source from the provider (`keychain:SERVICE` / `env:VAR` / `literal:` / `$(shell ...)`).
    pub apikey_source: Option<String>,
    /// Maximum tokens a single response may emit. `None` = consumer derives
    /// a heuristic from the context window.
    pub max_output_tokens: Option<u64>,
    /// Model context window size in tokens. `None` = consumer falls back to the
    /// id bracket suffix.
    pub max_tokens: Option<u64>,
    /// Whether the model accepts tool definitions. `true` when the model did
    /// not opt out, so existing models keep tool access by default.
    pub supports_tools: bool,
    /// Whether the model can ingest image content. `false` unless the model
    /// opts in, so models must declare vision explicitly.
    pub supports_images: bool,
}

impl ResolvedModel {
    /// Build a resolved model with shared semantics: `visible_agents` is the
    /// effective set for this model (wire_api-compatible agents, filtered by the
    /// endpoint + model `agents` lists, empty filter = no restriction), `env`
    /// merges provider base + model overrides (model takes precedence), defaults
    /// empty. This is the single source of truth — cx and manox both resolve
    /// through it, so the cascade wizard (manox) and cx's TUI see the same
    /// agent/model compatibility.
    fn from_config(
        config: &CxConfig,
        provider: &ProviderConfig,
        endpoint: &EndpointConfig,
        model: &ModelConfig,
    ) -> Self {
        let model_wire_apis: Vec<WireApi> = if model.wire_apis.is_empty() {
            vec![WireApi::from_str(&endpoint.wire_api)]
        } else {
            model
                .wire_apis
                .iter()
                .map(|s| WireApi::from_str(s))
                .filter(|w| *w != WireApi::Unavailable)
                .collect()
        };
        // Merged env: provider is the base, model entries override.
        let mut merged_env = provider.env.clone();
        merged_env.extend(model.env.clone());

        Self {
            id: model.id.clone(),
            desc: model.desc.clone().unwrap_or_default(),
            wire_api: WireApi::from_str(&endpoint.wire_api),
            model_wire_apis,
            provider_name: provider.name.clone(),
            endpoint_url: endpoint.url.clone(),
            visible_agents: effective_agents_for_model(config, provider, endpoint, model),
            copilot_auth: CopilotAuth::from_endpoint(endpoint),
            env: merged_env,
            apikey_source: provider.apikey_source.clone(),
            max_output_tokens: model.max_output_tokens,
            max_tokens: model.max_tokens,
            supports_tools: model.supports_tools.unwrap_or(true),
            supports_images: model.supports_images.unwrap_or(false),
        }
    }

    /// Stable id used for display and unique identification (provider/model/wire).
    pub fn key(&self) -> String {
        format!(
            "{}/{}/{}",
            self.provider_name,
            self.id,
            self.wire_api.display()
        )
    }

    /// model id sent to the API: the trailing `[<digits><unit?>]` context suffix is stripped (e.g. `glm-5.2[1m]` → `glm-5.2`).
    pub fn api_model_id(&self) -> String {
        strip_context_suffix(&self.id)
    }
}

/// Parse a context-window string into a token count.
///
/// Grammar: one or more terms of `<digits><unit?>` concatenated with no
/// separator, where `unit` ∈ {`k`,`K`,`m`,`M`} (case-insensitive, decimal
/// radix: `k` = 1_000, `m` = 1_000_000), summed. `1m123k` = 1_123_000,
/// `1m1234k` = 2_234_000 (pure sum, no magnitude cap). Integers only;
/// `0` accepted; leading zeros (`01m`) rejected; any internal whitespace
/// (`1 m`) rejected; surrounding whitespace trimmed. Any deviation → `None`.
pub fn parse_context_window(s: &str) -> Option<u64> {
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
        // A term must start with a digit.
        if !b[i].is_ascii_digit() {
            return None;
        }
        // Read the digit run.
        let mut j = i;
        while j < n && b[j].is_ascii_digit() {
            j += 1;
        }
        let digits = &b[i..j];
        // Leading-zero rule: a multi-digit run must not start with '0'.
        if digits.len() > 1 && digits[0] == b'0' {
            return None;
        }
        i = j;
        // Optional single unit letter; absence means a bare-number term.
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
                // Not a unit: the term is a bare number. The char is
                // re-examined as the next term's start; if it is a
                // non-digit the loop rejects on the next pass.
                _ => 1,
            }
        } else {
            1
        };
        let val: u64 = std::str::from_utf8(digits).ok()?.parse().ok()?;
        total = total.checked_add(val.checked_mul(mult)?)?;
        saw_term = true;
    }
    if !saw_term {
        return None;
    }
    Some(total)
}

/// Byte index of the `[` opening a single trailing `[...]` group, or `None`
/// when the id has no trailing group or has multiple / non-trailing brackets.
fn trailing_bracket_open(id: &str) -> Option<usize> {
    if !id.ends_with(']') {
        return None;
    }
    let open = id.rfind('[')?;
    // A single trailing group requires the prefix to contain no bracket of
    // its own, otherwise the id carries multiple groups (e.g. `[1m][2m]`).
    let prefix = &id[..open];
    if prefix.contains('[') || prefix.contains(']') {
        return None;
    }
    Some(open)
}

/// Extract a single trailing `[...]` context-window group from a model id and
/// parse it. Multi-group or non-trailing groups yield `None` (caller leaves
/// the id intact, so the bracket is sent to the API verbatim).
pub fn context_window_from_suffix(id: &str) -> Option<u64> {
    let open = trailing_bracket_open(id)?;
    let inner = &id[open + 1..id.len() - 1];
    parse_context_window(inner)
}

/// Strip a trailing `[...]` context-window suffix from a model id
/// (e.g. `glm-5.2[1m]` → `glm-5.2`, `glm-5.2[1m123k]` → `glm-5.2`). The
/// suffix must parse via [`parse_context_window`]; otherwise the id is
/// returned unchanged so an unparseable bracket reaches the API verbatim.
pub fn strip_context_suffix(id: &str) -> String {
    let Some(open) = trailing_bracket_open(id) else {
        return id.to_string();
    };
    let inner = &id[open + 1..id.len() - 1];
    if parse_context_window(inner).is_some() {
        id[..open].to_string()
    } else {
        id.to_string()
    }
}

impl CxConfig {
    /// Load from `~/.config/cx/cx.providers.config.yaml` (with legacy `config.yaml` migration).
    pub fn load_default() -> Result<Self> {
        let path = active_provider_config_path()?;
        read_config_file(&path)
    }

    /// Cross-normalize all providers, yielding every `ResolvedModel` (manox semantics).
    pub fn resolve_all_models(&self) -> Vec<ResolvedModel> {
        let mut out = Vec::new();
        for provider in &self.providers {
            for endpoint in provider.normalized_endpoints() {
                for model in &endpoint.models {
                    out.push(ResolvedModel::from_config(self, provider, &endpoint, model));
                }
            }
        }
        out
    }
}

impl FromStr for CxConfig {
    type Err = serde_yaml::Error;
    fn from_str(text: &str) -> std::result::Result<Self, Self::Err> {
        serde_yaml::from_str(text)
    }
}

// ═══════════════════════════════════════════════════
// Path resolution + legacy migration
// ═══════════════════════════════════════════════════

/// `$HOME/.config/cx`.
pub fn cx_state_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("无法解析用户主目录")?;
    Ok(home.join(".config/cx"))
}

/// Default cx config path: `$HOME/.config/cx/cx.providers.config.yaml`.
pub fn default_config_path() -> Result<PathBuf> {
    Ok(cx_state_dir()?.join(PROVIDER_CONFIG_FILE_NAME))
}

fn legacy_provider_config_path() -> Result<PathBuf> {
    Ok(cx_state_dir()?.join(LEGACY_PROVIDER_CONFIG_FILE_NAME))
}

/// Rename a legacy `config.yaml` to the current name if the current file is absent.
pub fn migrate_legacy_provider_config(current_path: &Path, legacy_path: &Path) -> Result<bool> {
    if current_path.exists() || !legacy_path.exists() {
        return Ok(false);
    }

    if let Some(parent) = current_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建配置目录失败: {}", parent.display()))?;
    }

    std::fs::rename(legacy_path, current_path).with_context(|| {
        format!(
            "迁移旧配置失败: {} -> {}",
            legacy_path.display(),
            current_path.display()
        )
    })?;
    eprintln!("已将旧 Provider 配置迁移到 {}", current_path.display());
    Ok(true)
}

/// Active config path, migrating a legacy file first if needed.
pub fn active_provider_config_path() -> Result<PathBuf> {
    let current_path = default_config_path()?;
    let legacy_path = legacy_provider_config_path()?;
    migrate_legacy_provider_config(&current_path, &legacy_path)?;
    Ok(current_path)
}

/// Read and parse a config file.
pub fn read_config_file(path: &Path) -> Result<CxConfig> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("读取配置文件失败: {}", path.display()))?;
    serde_yaml::from_str(&content).with_context(|| format!("解析配置文件失败: {}", path.display()))
}

// ═══════════════════════════════════════════════════
// apikey_source resolution
// ═══════════════════════════════════════════════════

/// Resolve an `apikey_source` into the actual API key string.
pub fn resolve_apikey(source: &str) -> Result<String> {
    if let Some(rest) = source.strip_prefix("keychain:") {
        keychain_secret(rest)
    } else if let Some(rest) = source.strip_prefix("env:") {
        std::env::var(rest).with_context(|| format!("环境变量 `{rest}` 未设置"))
    } else if let Some(rest) = source.strip_prefix("literal:") {
        Ok(rest.to_string())
    } else if source.starts_with("$(") {
        // Shell command: $(command)
        let cmd = source.trim_start_matches("$(").trim_end_matches(')');
        let output = Command::new("sh")
            .args(["-c", cmd])
            .output()
            .with_context(|| format!("执行 shell 命令失败: {cmd}"))?;
        if !output.status.success() {
            bail!("shell 命令 `{cmd}` 执行失败");
        }
        Ok(String::from_utf8(output.stdout)?.trim().to_string())
    } else {
        bail!("不支持的 apikey_source 格式: `{source}`")
    }
}

/// Read a generic password from the macOS Keychain.
fn keychain_secret(service: &str) -> Result<String> {
    if !cfg!(target_os = "macos") {
        bail!("`keychain:` 仅支持 macOS Keychain，请改用 `env:` 配置 `{service}`。");
    }

    let user = std::env::var("USER").unwrap_or_default();
    let output = Command::new("security")
        .args(["find-generic-password", "-a", &user, "-s", service, "-w"])
        .output()
        .with_context(|| format!("调用 macOS Keychain 失败，service={service}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        bail!(
            "无法从 Keychain 读取 `{service}`: {}",
            if stderr.is_empty() {
                "未知错误".to_string()
            } else {
                stderr
            }
        );
    }

    Ok(String::from_utf8(output.stdout)?
        .trim_end_matches(['\n', '\r'])
        .to_string())
}

// ═══════════════════════════════════════════════════
// Agent resolution
// ═══════════════════════════════════════════════════

/// A fully resolved, launchable agent. Built from `AgentConfig` (user YAML) plus
/// built-in hidden agents (e.g. `codex+`) that `resolved_agents` appends
/// unconditionally. `cx` consumes this for TUI selection + launch wiring; the
/// `visible_agents` field of `ResolvedModel` is derived from it via
/// `effective_agents_for_model`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAgent {
    pub id: String,
    pub binary: String,
    pub args: Vec<String>,
    pub supported_wire_apis: Vec<WireApi>,
    pub env: BTreeMap<String, String>,
    /// Built-in hidden agent (e.g. `codex+`): not shown in the default agent list,
    /// surfaced only on explicit `cx <id>`. Appended by `resolved_agents`, never
    /// written to user config.
    pub hidden: bool,
}

impl ResolvedAgent {
    pub fn supports_wire_api(&self, wire_api: WireApi) -> bool {
        self.supported_wire_apis.contains(&wire_api)
    }
}

/// Canonicalize a config/registry agent id. The only remap is the legacy
/// `codex-app` → `codex` (the CLI no longer proxies `cx codex app`).
pub fn canonical_agent_id(agent_id: &str) -> &str {
    match agent_id {
        // Backward-compat for legacy config entries only; the CLI no longer proxies `codex app`.
        "codex-app" => "codex",
        _ => agent_id,
    }
}

/// Normalize a list of agent ids: apply `canonical_agent_id`, dedupe preserving
/// first-seen order. Used by `effective_agents_for_model` to compare filters.
pub fn normalize_agent_ids(agent_ids: &[String]) -> Vec<String> {
    let mut normalized = Vec::new();
    for agent_id in agent_ids {
        let canonical = canonical_agent_id(agent_id).to_string();
        if !normalized.iter().any(|existing| existing == &canonical) {
            normalized.push(canonical);
        }
    }
    normalized
}

/// Hardcoded wire_apis each built-in agent supports. The config file's per-agent
/// `wire_apis` field is ignored — these are the source of truth.
pub fn default_wire_apis_for_agent(agent_id: &str) -> Vec<WireApi> {
    match canonical_agent_id(agent_id) {
        "copilot" => vec![WireApi::Anthropic, WireApi::Responses, WireApi::Completions],
        "claude" => vec![WireApi::Anthropic],
        "codex" | "Codex.app" => vec![WireApi::Responses],
        // codex+ is a codex fork that additionally supports anthropic / completions.
        // Only entered on explicit `cx codex+`; not written to the default agents list.
        "codex+" => vec![WireApi::Anthropic, WireApi::Responses, WireApi::Completions],
        _ => Vec::new(),
    }
}

/// Resolve an agent's wire_apis. Configured `wire_apis` are ignored in favor of
/// the hardcoded `default_wire_apis_for_agent`.
pub fn resolve_agent_wire_apis(agent_id: &str, _configured: &[String]) -> Vec<WireApi> {
    default_wire_apis_for_agent(agent_id)
}

/// Built-in hidden agents: not written to user YAML, appended unconditionally by
/// `resolved_agents` (unless the user already defined a same-named entry, which
/// takes precedence and un-hides it).
pub fn builtin_hidden_agent_configs() -> Vec<AgentConfig> {
    vec![AgentConfig {
        id: "codex+".into(),
        binary: "codex+".into(),
        args: Vec::new(),
        wire_apis: vec!["anthropic".into(), "responses".into(), "completions".into()],
        env: BTreeMap::new(),
    }]
}

/// All resolved agents: user-configured agents (with `codex` expanded into `codex`
/// CLI + `Codex.app` desktop entries) plus built-in hidden agents appended
/// unconditionally (skipped if the user already defined a same-named entry).
pub fn resolved_agents(config: &CxConfig) -> Vec<ResolvedAgent> {
    let mut agents = Vec::new();
    for agent in &config.agents {
        let id = canonical_agent_id(&agent.id).to_string();
        if agents
            .iter()
            .any(|existing: &ResolvedAgent| existing.id == id)
        {
            continue;
        }
        let supported_wire_apis = resolve_agent_wire_apis(&id, &agent.wire_apis);

        if id == "codex" {
            // codex expands into a CLI entry and a Desktop App entry.
            agents.push(ResolvedAgent {
                id: "codex".into(),
                binary: "codex".into(),
                args: vec![],
                supported_wire_apis: supported_wire_apis.clone(),
                env: agent.env.clone(),
                hidden: false,
            });
            agents.push(ResolvedAgent {
                id: "Codex.app".into(),
                binary: "codex".into(),
                args: vec!["app".into()],
                supported_wire_apis,
                env: agent.env.clone(),
                hidden: false,
            });
        } else {
            agents.push(ResolvedAgent {
                id,
                binary: agent.binary.clone(),
                args: agent.args.clone(),
                supported_wire_apis,
                env: agent.env.clone(),
                hidden: false,
            });
        }
    }

    // Append built-in hidden agents (not in user YAML). Skip if the user already
    // defined a same-named entry, to respect their explicit config (which un-hides it).
    for builtin in builtin_hidden_agent_configs() {
        let id = canonical_agent_id(&builtin.id).to_string();
        if agents
            .iter()
            .any(|existing: &ResolvedAgent| existing.id == id)
        {
            continue;
        }
        agents.push(ResolvedAgent {
            id: id.clone(),
            binary: builtin.binary.clone(),
            args: builtin.args.clone(),
            supported_wire_apis: default_wire_apis_for_agent(&id),
            env: builtin.env.clone(),
            hidden: true,
        });
    }

    agents
}

/// Agents whose wire_apis include the endpoint's `wire_api`. The compatibility
/// baseline for `effective_agents_for_model` before the endpoint/model filters apply.
pub fn all_compatible_agents(config: &CxConfig, endpoint: &EndpointConfig) -> Vec<String> {
    let wire_api = WireApi::from_str(&endpoint.wire_api);
    resolved_agents(config)
        .into_iter()
        .filter(|agent| agent.supports_wire_api(wire_api))
        .map(|agent| agent.id)
        .collect()
}

/// The effective set of agents a model supports — the single source of truth
/// consumed by `ResolvedModel.visible_agents`.
///
/// Starts from `all_compatible_agents` (agents whose hardcoded wire_apis match
/// the endpoint's wire_api), then applies two optional allow-list filters in
/// order: the endpoint's `agents`, then the model's `agents`. An empty filter is
/// **not** a restriction — it is skipped (no-op), meaning "no explicit allow-list,
/// inherit the prior set". A non-empty filter retains only the listed agents. This
/// makes a model with neither endpoint nor model `agents` support every
/// wire_api-compatible agent, while a model with `agents: [claude]` narrows to
/// claude only.
pub fn effective_agents_for_model(
    config: &CxConfig,
    _provider: &ProviderConfig,
    endpoint: &EndpointConfig,
    model: &ModelConfig,
) -> Vec<String> {
    let mut resolved = all_compatible_agents(config, endpoint);

    for filter in [&endpoint.agents, &model.agents] {
        if filter.is_empty() {
            continue;
        }

        let allowed = normalize_agent_ids(filter);
        resolved.retain(|agent_id| allowed.iter().any(|allowed_id| allowed_id == agent_id));
    }

    resolved
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sample_config() {
        let yaml = r#"
providers:
- name: 百炼
  apikey_source: keychain:DASHSCOPE_API_KEY
  models:
    glm-5.2:
      desc: 智谱旗舰
      wire_apis: [anthropic]
  endpoints:
    anthropic:
      url: https://example.com/anthropic
"#;
        let config: CxConfig = yaml.parse().expect("parse");
        assert_eq!(config.providers.len(), 1);
        assert!(config.agents.is_empty());
        let resolved = config.resolve_all_models();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].id, "glm-5.2");
        assert_eq!(resolved[0].wire_api, WireApi::Anthropic);
        assert_eq!(resolved[0].provider_name, "百炼");
    }

    #[test]
    fn resolve_merges_provider_and_model_env() {
        let yaml = r#"
providers:
- name: test
  env:
    SHARED: from-provider
    PROVIDER_ONLY: pv
  models:
    m1:
      env:
        SHARED: from-model
        MODEL_ONLY: mv
      wire_apis: [anthropic]
  endpoints:
    anthropic:
      url: https://example.com
"#;
        let config: CxConfig = yaml.parse().expect("parse");
        let resolved = config.resolve_all_models();
        assert_eq!(resolved.len(), 1);
        // Model env overrides provider env for shared key
        assert_eq!(
            resolved[0].env.get("SHARED"),
            Some(&"from-model".to_string())
        );
        // Both unique keys are present
        assert_eq!(
            resolved[0].env.get("PROVIDER_ONLY"),
            Some(&"pv".to_string())
        );
        assert_eq!(resolved[0].env.get("MODEL_ONLY"), Some(&"mv".to_string()));
    }

    #[test]
    fn resolve_provider_env_without_model_env() {
        let yaml = r#"
providers:
- name: test
  env:
    KEY1: v1
    KEY2: v2
  models:
    m1:
      wire_apis: [anthropic]
  endpoints:
    anthropic:
      url: https://example.com
"#;
        let config: CxConfig = yaml.parse().expect("parse");
        let resolved = config.resolve_all_models();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].env.get("KEY1"), Some(&"v1".to_string()));
        assert_eq!(resolved[0].env.get("KEY2"), Some(&"v2".to_string()));
    }

    #[test]
    fn load_real_config_if_present() {
        let path = match default_config_path() {
            Ok(p) if p.exists() => p,
            _ => return, // Skip when no cx config is present on this machine.
        };
        let config = CxConfig::load_default().expect("load");
        assert!(!config.providers.is_empty(), "至少应有一个 provider");
        let resolved = config.resolve_all_models();
        assert!(!resolved.is_empty(), "至少应有一个 resolved model");
        let _ = path;
    }

    #[test]
    fn parse_context_window_cases() {
        // Accepted (decimal radix, pure sum).
        assert_eq!(parse_context_window("1m"), Some(1_000_000));
        assert_eq!(parse_context_window("244k"), Some(244_000));
        assert_eq!(parse_context_window("208000"), Some(208_000));
        assert_eq!(parse_context_window("1m123k"), Some(1_123_000));
        assert_eq!(parse_context_window("1m1234k"), Some(2_234_000));
        assert_eq!(parse_context_window("1M"), Some(1_000_000));
        assert_eq!(parse_context_window("244K"), Some(244_000));
        assert_eq!(parse_context_window("1m123K"), Some(1_123_000));
        assert_eq!(parse_context_window("0"), Some(0));
        assert_eq!(parse_context_window("0k"), Some(0));
        assert_eq!(parse_context_window("2m0k"), Some(2_000_000));
        assert_eq!(parse_context_window(" 1m "), Some(1_000_000));
        assert_eq!(parse_context_window("8192"), Some(8192));

        // Rejected → None.
        assert_eq!(parse_context_window("01m"), None); // leading zero
        assert_eq!(parse_context_window("1 m"), None); // internal whitespace
        assert_eq!(parse_context_window("1.5m"), None); // decimal
        assert_eq!(parse_context_window(""), None);
        assert_eq!(parse_context_window("   "), None);
        assert_eq!(parse_context_window("garbage"), None);
        assert_eq!(parse_context_window("1g"), None); // unknown unit
        assert_eq!(parse_context_window("1m2"), Some(1_000_002)); // m + bare 2
    }

    #[test]
    fn context_window_from_suffix_cases() {
        assert_eq!(context_window_from_suffix("glm-5.2[1m]"), Some(1_000_000));
        assert_eq!(
            context_window_from_suffix("deepseek-v4-pro[1m1234k]"),
            Some(2_234_000)
        );
        assert_eq!(context_window_from_suffix("plain-model"), None);
        assert_eq!(context_window_from_suffix("bad[]"), None);
        assert_eq!(context_window_from_suffix("bad[abc]"), None);
        assert_eq!(context_window_from_suffix("no-suffix[1m"), None); // unclosed
        assert_eq!(context_window_from_suffix("glm[1m]5.2"), None); // non-trailing
        assert_eq!(context_window_from_suffix("[1m][2m]"), None); // multi-group
        assert_eq!(context_window_from_suffix("[01m]"), None); // leading zero
    }

    #[test]
    fn strip_context_suffix_cases() {
        // Single-term (unchanged from before).
        assert_eq!(strip_context_suffix("glm-5.2[1m]"), "glm-5.2");
        assert_eq!(strip_context_suffix("qwen3.7-plus[200k]"), "qwen3.7-plus");
        assert_eq!(strip_context_suffix("plain-model"), "plain-model");
        assert_eq!(strip_context_suffix("no-suffix[1m"), "no-suffix[1m");
        assert_eq!(strip_context_suffix("bad[]"), "bad[]");
        assert_eq!(strip_context_suffix("bad[abc]"), "bad[abc]");
        assert_eq!(strip_context_suffix("num-only[128]"), "num-only");

        // Multi-term duration-style now strips.
        assert_eq!(strip_context_suffix("glm-5.2[1m123k]"), "glm-5.2");
        assert_eq!(strip_context_suffix("glm-5.2[1m1234k]"), "glm-5.2");
        assert_eq!(strip_context_suffix("m[1M]"), "m");
        assert_eq!(strip_context_suffix("m[0]"), "m");

        // Rejections leave the bracket verbatim.
        assert_eq!(strip_context_suffix("m[01m]"), "m[01m]"); // leading zero
        assert_eq!(strip_context_suffix("m[1 m]"), "m[1 m]"); // internal space
        assert_eq!(strip_context_suffix("m[1.5m]"), "m[1.5m]"); // decimal
        assert_eq!(strip_context_suffix("m[1g]"), "m[1g]"); // unknown unit
        assert_eq!(strip_context_suffix("a[1m][2m]"), "a[1m][2m]"); // multi-group
        assert_eq!(strip_context_suffix("glm[1m]5.2"), "glm[1m]5.2"); // non-trailing
    }

    #[test]
    fn migrate_legacy_provider_config_moves_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let current_path = dir.path().join(PROVIDER_CONFIG_FILE_NAME);
        let legacy_path = dir.path().join(LEGACY_PROVIDER_CONFIG_FILE_NAME);
        let content = "providers: []\nagents: []\n";
        std::fs::write(&legacy_path, content).unwrap();

        let migrated = migrate_legacy_provider_config(&current_path, &legacy_path).unwrap();
        assert!(migrated);
        assert!(!legacy_path.exists());
        assert_eq!(std::fs::read_to_string(&current_path).unwrap(), content);
    }

    #[test]
    fn migrate_legacy_provider_config_noop_when_current_exists() {
        let dir = tempfile::tempdir().expect("tempdir");
        let current_path = dir.path().join(PROVIDER_CONFIG_FILE_NAME);
        let legacy_path = dir.path().join(LEGACY_PROVIDER_CONFIG_FILE_NAME);
        std::fs::write(&current_path, "providers: []\n").unwrap();
        std::fs::write(&legacy_path, "providers: []\n").unwrap();

        let migrated = migrate_legacy_provider_config(&current_path, &legacy_path).unwrap();
        assert!(!migrated);
        assert!(legacy_path.exists());
    }

    #[test]
    fn resolve_literal() {
        assert_eq!(resolve_apikey("literal:sk-abc").unwrap(), "sk-abc");
    }

    #[test]
    fn resolve_env() {
        // HOME is set on every platform.
        let v = resolve_apikey("env:HOME").expect("env:HOME 应成功");
        assert!(!v.is_empty());
    }

    #[test]
    fn resolve_shell() {
        let v = resolve_apikey("$(echo hello)").unwrap();
        assert_eq!(v.trim(), "hello");
    }

    #[test]
    fn resolve_unsupported() {
        assert!(resolve_apikey("foo:bar").is_err());
    }

    /// `visible_agents` reflects the effective agent set, not just
    /// `endpoint.agents`. A model-level `agents: [claude]` with an empty endpoint
    /// `agents` must resolve to `[claude]` — the pre-fix "manox semantics" copied
    /// only `endpoint.agents` and yielded `[]`, hiding every model from the
    /// cascade wizard.
    #[test]
    fn visible_agents_model_level_filter_narrows_to_claude() {
        let yaml = r#"
providers:
- name: test
  models:
    m1:
      wire_apis: [anthropic]
      agents: [claude]
  endpoints:
    anthropic:
      url: https://example.com
agents:
- id: claude
  binary: claude
  wire_apis: [anthropic]
- id: codex
  binary: codex
  wire_apis: [responses]
- id: copilot
  binary: copilot
  wire_apis: [anthropic, responses, completions]
"#;
        let config: CxConfig = yaml.parse().expect("parse");
        let resolved = config.resolve_all_models();
        assert_eq!(resolved.len(), 1);
        assert_eq!(
            resolved[0].visible_agents,
            vec!["claude".to_string()],
            "model-level agents: [claude] narrows the effective set to [claude]"
        );
    }

    /// With neither endpoint nor model `agents`, a model supports every
    /// wire_api-compatible agent (empty filter = no restriction, not "no agents").
    /// The pre-fix semantics returned `[]` here too.
    #[test]
    fn visible_agents_empty_filters_mean_all_compatible() {
        let yaml = r#"
providers:
- name: test
  models:
    m1:
      wire_apis: [anthropic]
  endpoints:
    anthropic:
      url: https://example.com
agents:
- id: claude
  binary: claude
  wire_apis: [anthropic]
- id: codex
  binary: codex
  wire_apis: [responses]
- id: copilot
  binary: copilot
  wire_apis: [anthropic, responses, completions]
"#;
        let config: CxConfig = yaml.parse().expect("parse");
        let resolved = config.resolve_all_models();
        assert_eq!(resolved.len(), 1);
        // claude + copilot support the anthropic wire; codex (responses-only) does not.
        // codex+ (builtin hidden) also supports anthropic, so it appears too.
        assert!(
            resolved[0].visible_agents.contains(&"claude".to_string()),
            "empty filters must not zero out visible_agents; got {:?}",
            resolved[0].visible_agents
        );
        assert!(
            !resolved[0].visible_agents.is_empty(),
            "empty filters mean all wire_api-compatible agents, not none"
        );
        assert!(
            !resolved[0].visible_agents.contains(&"codex".to_string()),
            "codex (responses-only) must not be compatible with the anthropic endpoint"
        );
    }

    /// An endpoint-level `agents` filter restricts every model on that endpoint.
    #[test]
    fn visible_agents_endpoint_filter_restricts_models() {
        let yaml = r#"
providers:
- name: test
  models:
    m1:
      wire_apis: [anthropic]
  endpoints:
    anthropic:
      url: https://example.com
      agents: [claude]
agents:
- id: claude
  binary: claude
  wire_apis: [anthropic]
- id: copilot
  binary: copilot
  wire_apis: [anthropic, responses, completions]
"#;
        let config: CxConfig = yaml.parse().expect("parse");
        let resolved = config.resolve_all_models();
        assert_eq!(resolved.len(), 1);
        assert_eq!(
            resolved[0].visible_agents,
            vec!["claude".to_string()],
            "endpoint agents: [claude] restricts the effective set to [claude]"
        );
    }

    /// Explicit capability/sizing fields on a model flow through to the
    /// resolved model verbatim, so manox can read ground-truth modality and
    /// token budgets instead of relying on model self-report or heuristics.
    #[test]
    fn resolve_propagates_capability_fields() {
        let yaml = r#"
providers:
- name: test
  models:
    m1:
      wire_apis: [anthropic]
      max_output_tokens: 8192
      max_tokens: 200000
      supports_tools: false
      supports_images: true
  endpoints:
    anthropic:
      url: https://example.com
"#;
        let config: CxConfig = yaml.parse().expect("parse");
        let resolved = config.resolve_all_models();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].max_output_tokens, Some(8192));
        assert_eq!(resolved[0].max_tokens, Some(200_000));
        assert!(!resolved[0].supports_tools);
        assert!(resolved[0].supports_images);
    }

    /// Omitted capability fields resolve to safe defaults: tools on (existing
    /// reasoning/chat models keep tool access), images off (models opt in to
    /// vision), token budgets unset (consumer derives from context/suffix).
    #[test]
    fn resolve_defaults_capability_fields() {
        let yaml = r#"
providers:
- name: test
  models:
    m1:
      wire_apis: [anthropic]
  endpoints:
    anthropic:
      url: https://example.com
"#;
        let config: CxConfig = yaml.parse().expect("parse");
        let resolved = config.resolve_all_models();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].max_output_tokens, None);
        assert_eq!(resolved[0].max_tokens, None);
        assert!(resolved[0].supports_tools, "tools default on");
        assert!(!resolved[0].supports_images, "images default off");
    }
}
