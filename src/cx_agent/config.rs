//! Selection → ProviderAdapter 配置 映射。
//!
//! 把 cx 的 (provider × wire_api × model_id) 转成 rig-core 客户端能消费的
//! `(api_key, base_url, model_id, wire_api)` 四元组。

use std::fs;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

use crate::cx_agent::approval::ApprovalMode;
use crate::{ResolvedProvider, Selection, WireApi};

/// 实例化 ProviderAdapter 所需的最小信息。
#[derive(Debug, Clone)]
pub struct ProviderAdapterConfig {
    pub provider_name: String,
    pub model_id: String,
    pub wire_api: WireApi,
    pub base_url: String,
    pub api_key: String,
}

impl ProviderAdapterConfig {
    pub fn from_selection(selection: &Selection) -> Result<Self> {
        let model = selection
            .model
            .as_ref()
            .ok_or_else(|| anyhow!("Cx Agent 需要先在 launcher 中选择具体 model"))?;

        let api_key = resolve_provider_apikey(&selection.provider).with_context(|| {
            format!("解析 provider `{}` 的 apikey 失败", selection.provider.name)
        })?;

        if !matches!(
            model.wire_api,
            WireApi::Responses | WireApi::Completions | WireApi::Anthropic
        ) {
            bail!(
                "Cx Agent 不支持 wire_api={}（model={}）",
                model.wire_api.display(),
                model.id
            );
        }

        Ok(Self {
            provider_name: selection.provider.name.clone(),
            model_id: model.id.clone(),
            wire_api: model.wire_api,
            base_url: model.endpoint_url.clone(),
            api_key,
        })
    }
}

/// v0 Cx Agent 的运行参数。
#[derive(Debug, Clone)]
pub struct CxAgentRuntimeConfig {
    pub adapter: ProviderAdapterConfig,
    pub approval_mode: ApprovalMode,
}

impl CxAgentRuntimeConfig {
    pub fn from_selection(selection: &Selection) -> Result<Self> {
        Ok(Self {
            adapter: ProviderAdapterConfig::from_selection(selection)?,
            approval_mode: load_approval_mode()?,
        })
    }
}

fn resolve_provider_apikey(provider: &ResolvedProvider) -> Result<String> {
    let source = provider
        .apikey_source
        .as_deref()
        .ok_or_else(|| anyhow!("provider `{}` 缺少 apikey_source", provider.name))?;
    crate::resolve_apikey(source)
}

#[derive(Debug, Deserialize, Default)]
struct RawCxConfig {
    #[serde(default)]
    cx_agent: RawCxAgentConfig,
}

#[derive(Debug, Deserialize, Default)]
struct RawCxAgentConfig {
    #[serde(default)]
    approval_mode: Option<String>,
}

fn load_approval_mode() -> Result<ApprovalMode> {
    let path = crate::active_provider_config_path()?;
    if !path.exists() {
        return Ok(ApprovalMode::default());
    }

    let content = fs::read_to_string(&path)
        .with_context(|| format!("读取 Cx Agent 配置失败: {}", path.display()))?;
    let raw: RawCxConfig = serde_yaml::from_str(&content)
        .with_context(|| format!("解析 Cx Agent 配置失败: {}", path.display()))?;

    match raw.cx_agent.approval_mode.as_deref() {
        None => Ok(ApprovalMode::default()),
        Some(value) => parse_approval_mode(value),
    }
}

fn parse_approval_mode(raw: &str) -> Result<ApprovalMode> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "always-allow" | "always_allow" => Ok(ApprovalMode::AlwaysAllow),
        "per-call" | "per_call" => Ok(ApprovalMode::PerCall),
        "read-only-auto-allow" | "read_only_auto_allow" => Ok(ApprovalMode::ReadOnlyAutoAllow),
        other => Err(anyhow!(
            "不支持的 cx_agent.approval_mode={other}，可选值: always-allow / per-call / read-only-auto-allow"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::{ApprovalMode, parse_approval_mode};

    #[test]
    fn parses_supported_approval_modes() {
        assert_eq!(
            parse_approval_mode("always-allow").unwrap(),
            ApprovalMode::AlwaysAllow
        );
        assert_eq!(
            parse_approval_mode("per_call").unwrap(),
            ApprovalMode::PerCall
        );
        assert_eq!(
            parse_approval_mode("read-only-auto-allow").unwrap(),
            ApprovalMode::ReadOnlyAutoAllow
        );
    }

    #[test]
    fn rejects_unknown_approval_modes() {
        let err = parse_approval_mode("surprise-mode").unwrap_err();
        assert!(err.to_string().contains("cx_agent.approval_mode"));
    }
}
