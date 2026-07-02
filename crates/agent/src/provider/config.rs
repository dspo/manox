//! cx providers config parsing.
//!
//! Config file: `~/.config/cx/cx.providers.config.yaml`. Schema:
//! - `providers: Vec<ProviderConfig>`, each with `name` / `apikey_source` /
//!   `models: BTreeMap<id, ProviderModelConfig>` / `endpoints: BTreeMap<wire_api, spec>` / `env`.
//! - Each model supports several `wire_apis` (anthropic / responses / completions).
//! - `ResolvedModel` is a fully resolved, callable model (provider + endpoint + wire_api + auth).

use std::collections::BTreeMap;
use std::str::FromStr;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

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

    /// anthropic(0) > responses(1) > completions(2) > unavailable(3)。
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

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct CxConfig {
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
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
        format!("{}/{}/{}", self.provider_name, self.id, self.wire_api.display())
    }

    /// model id sent to the API: the trailing `[<digits>m]` context suffix is stripped (e.g. `glm-5.2[1m]` → `glm-5.2`).
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
    /// Load from `~/.config/cx/cx.providers.config.yaml`.
    pub fn load_default() -> Result<Self> {
        let path = default_config_path()?;
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("读取 cx providers 配置失败: {}", path.display()))?;
        text.parse().context("解析 cx providers 配置失败")
    }

    /// Cross-normalize all providers, yielding every `ResolvedModel`.
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

/// Default cx config path: `$HOME/.config/cx/cx.providers.config.yaml`.
pub fn default_config_path() -> Result<std::path::PathBuf> {
    let home = std::env::var("HOME").map_err(|_| anyhow!("HOME 环境变量未设置"))?;
    Ok(std::path::PathBuf::from(home)
        .join(".config")
        .join("cx")
        .join("cx.providers.config.yaml"))
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
            _ => {
                // Skip when no cx config is present on this machine.
                return;
            }
        };
        let config = CxConfig::load_default().expect("load");
        assert!(!config.providers.is_empty(), "至少应有一个 provider");
        let resolved = config.resolve_all_models();
        assert!(!resolved.is_empty(), "至少应有一个 resolved model");
        // The Bailian provider should exist and contain glm-5.2[1m] (anthropic wire).
        let has_bailian_glm = resolved.iter().any(|m| {
            m.provider_name == "百炼" && m.id.contains("glm-5.2") && m.wire_api == WireApi::Anthropic
        });
        assert!(has_bailian_glm, "应含百炼 glm-5.2[1m] anthropic");
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
}
