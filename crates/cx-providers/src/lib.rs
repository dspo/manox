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

// ══════════════════════════════════════════════════
// Wire protocol + auth strategy
// ══════════════════════════════════════════════════

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

// ══════════════════════════════════════════════════
// Deserialization structs (mirror the YAML schema)
// ══════════════════════════════════════════════════

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
    pub swe_pro: Option<String>,
    #[serde(default)]
    pub hle: Option<String>,
    #[serde(default)]
    pub desc: Option<String>,
    #[serde(default)]
    pub context: Option<String>,
    #[serde(default)]
    pub wire_apis: Vec<String>,
    #[serde(default)]
    pub agents: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct ProviderModelConfig {
    #[serde(default)]
    pub swe_pro: Option<String>,
    #[serde(default)]
    pub hle: Option<String>,
    #[serde(default)]
    pub desc: Option<String>,
    #[serde(default)]
    pub context: Option<String>,
    #[serde(default)]
    pub wire_apis: Vec<String>,
    #[serde(default)]
    pub agents: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
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

// ══════════════════════════════════════════════════
// Normalization + resolution
// ══════════════════════════════════════════════════

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
                        swe_pro: model.swe_pro.clone(),
                        hle: model.hle.clone(),
                        desc: model.desc.clone(),
                        context: model.context.clone(),
                        wire_apis: model.wire_apis.clone(),
                        agents: model.agents.clone(),
                        env: model.env.clone(),
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
    pub swe_pro: String,
    pub hle: String,
    pub desc: String,
    pub context: String,
    pub wire_api: WireApi,
    pub model_wire_apis: Vec<WireApi>,
    pub provider_name: String,
    pub endpoint_url: String,
    pub visible_agents: Vec<String>,
    pub copilot_auth: CopilotAuth,
    pub env: BTreeMap<String, String>,
    /// apikey resolution source from the provider (`keychain:SERVICE` / `env:VAR` / `literal:` / `$(shell ...)`).
    pub apikey_source: Option<String>,
}

impl ResolvedModel {
    /// Build a resolved model with manox semantics: `visible_agents` = endpoint agents,
    /// `env` = model env only, defaults empty. cx post-processes (agent filtering, env merge)
    /// in its own `build_all_models` because its TUI needs cross-model agent compatibility.
    fn from_config(
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

        Self {
            id: model.id.clone(),
            swe_pro: model.swe_pro.clone().unwrap_or_default(),
            hle: model.hle.clone().unwrap_or_default(),
            desc: model.desc.clone().unwrap_or_default(),
            context: model.context.clone().unwrap_or_default(),
            wire_api: WireApi::from_str(&endpoint.wire_api),
            model_wire_apis,
            provider_name: provider.name.clone(),
            endpoint_url: endpoint.url.clone(),
            visible_agents: endpoint.agents.clone(),
            copilot_auth: CopilotAuth::from_endpoint(endpoint),
            env: model.env.clone(),
            apikey_source: provider.apikey_source.clone(),
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

/// Strip a trailing `[<digits><unit?>]` context-window suffix from a model id (e.g. `glm-5.2[1m]` → `glm-5.2`).
/// Non-regex scan: requires at least one digit after `[`, an optional single-letter unit,
/// a closing `]`, and the suffix at the end of the string.
pub fn strip_context_suffix(id: &str) -> String {
    if !id.ends_with(']') {
        return id.to_string();
    }
    let Some(open) = id.rfind('[') else {
        return id.to_string();
    };
    let inner = &id[open + 1..id.len() - 1];
    let mut chars = inner.chars();
    let Some(first) = chars.next() else {
        return id.to_string();
    };
    if !first.is_ascii_digit() {
        return id.to_string();
    }
    let mut unit: Option<char> = None;
    for c in chars {
        if c.is_ascii_digit() {
            continue;
        }
        if unit.is_none() && c.is_ascii_alphabetic() {
            unit = Some(c);
        } else {
            return id.to_string();
        }
    }
    id[..open].to_string()
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
                    out.push(ResolvedModel::from_config(provider, &endpoint, model));
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

// ══════════════════════════════════════════════════
// Path resolution + legacy migration
// ══════════════════════════════════════════════════

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

// ══════════════════════════════════════════════════
// apikey_source resolution
// ══════════════════════════════════════════════════

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
    fn strip_context_suffix_cases() {
        assert_eq!(strip_context_suffix("glm-5.2[1m]"), "glm-5.2");
        assert_eq!(strip_context_suffix("qwen3.7-plus[200k]"), "qwen3.7-plus");
        assert_eq!(strip_context_suffix("plain-model"), "plain-model");
        assert_eq!(strip_context_suffix("no-suffix[1m"), "no-suffix[1m");
        assert_eq!(strip_context_suffix("bad[]"), "bad[]");
        assert_eq!(strip_context_suffix("bad[abc]"), "bad[abc]");
        assert_eq!(strip_context_suffix("num-only[128]"), "num-only");
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
}
