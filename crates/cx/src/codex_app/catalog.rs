//! ChatGPT.app 引擎模型目录（`model_catalog_json`）生成器。
//!
//! 背景：Codex 引擎对未知模型 slug 走 fallback 元数据（`model_info_from_slug`，
//! `max_context_window = 272_000`），并把 config 的 `model_context_window` 钳制到该上限，
//! 再乘 95% 可用窗口 → ChatGPT.app 的 composer 恒显示 258k（258_400），
//! 与模型实际上下文规模无关。
//!
//! 解法：通过 config.toml 的 `model_catalog_json` 提供本地模型目录（`ModelsResponse`），
//! 让引擎「认识」这些自定义模型。这里只收录带显式 `[Nm]` 上下文后缀的模型
//! （如 `deepseek-v4-pro[1m]`），并把 `context_window`/`max_context_window` 设为后缀声明的值；
//! 无后缀的模型不进入目录，继续走引擎 fallback，避免对未知上下文规模的模型夸大窗口。
//!
//! 其余描述符字段对齐引擎 fallback（`model_info_from_slug`）：所有可省略字段
//! 直接省略、由 serde 默认值补齐，保证除上下文窗口外行为与现状完全一致；
//! 尤其 `base_instructions` 与 app 内置引擎当前捆绑的 Codex 系统提示词逐字节一致
//! （见 `base_instructions.txt`），避免改变注入模型的系统提示。

use crate::ResolvedModel;
use serde_json::{Value, json};

/// 与 ChatGPT.app 内置引擎 fallback 完全一致的 base instructions
/// （来自 app 内置 codex 引擎捆绑的 `prompt.md`，需随引擎版本同步）。
const BASE_INSTRUCTIONS: &str = include_str!("base_instructions.txt");

/// 构建 `{"models": [...]}` 模型目录。仅收录带 `[Nm]` 上下文后缀的模型。
pub fn build_model_catalog(models: &[ResolvedModel]) -> Value {
    let entries: Vec<Value> = models.iter().filter_map(model_entry).collect();
    json!({ "models": entries })
}

/// 单个模型条目：字段对齐引擎 fallback，仅覆盖上下文窗口。
/// 返回 `None` 表示该模型没有显式 `[Nm]` 后缀，不应进入目录。
fn model_entry(model: &ResolvedModel) -> Option<Value> {
    let (api_id, context) = crate::parse_model_context_suffix(&model.id);
    let context = context?;
    Some(json!({
        "slug": api_id,
        "display_name": api_id,
        "supported_reasoning_levels": [],
        "shell_type": "default",
        "visibility": "none",
        "supported_in_api": true,
        "priority": 99,
        "base_instructions": BASE_INSTRUCTIONS,
        "supports_reasoning_summaries": false,
        "support_verbosity": false,
        "truncation_policy": { "mode": "bytes", "limit": 10000 },
        "supports_parallel_tool_calls": false,
        "experimental_supported_tools": [],
        "context_window": context,
        "max_context_window": context,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CopilotAuth, ResolvedModel, WireApi};
    use std::collections::BTreeMap;

    fn rm(id: &str) -> ResolvedModel {
        ResolvedModel {
            id: id.into(),
            desc: String::new(),
            wire_api: WireApi::Responses,
            model_wire_apis: vec![WireApi::Responses],
            provider_name: "DashScope".into(),
            endpoint_url: "https://example.com/v1".into(),
            visible_agents: vec!["Codex.app".into()],
            copilot_auth: CopilotAuth::ApiKey,
            env: BTreeMap::new(),
            apikey_source: None,
            max_tokens: None,
            context: None,
            supports_tools: true,
            supports_images: false,
        }
    }

    #[test]
    fn catalog_includes_only_suffix_models_with_resolved_context() {
        let models = vec![rm("qwen3.7-max[1m]"), rm("glm-5.2[3m]"), rm("plain-model")];
        let catalog = build_model_catalog(&models);
        let entries = catalog["models"].as_array().unwrap();
        assert_eq!(entries.len(), 2, "无后缀模型不应进入目录");
        let by_slug: BTreeMap<_, _> = entries
            .iter()
            .map(|e| (e["slug"].as_str().unwrap(), e))
            .collect();
        // [Nm] 后缀被剥离，且 context/max_context_window 取后缀声明的值。
        assert!(by_slug.contains_key("qwen3.7-max"));
        assert_eq!(by_slug["qwen3.7-max"]["context_window"], 1_000_000);
        assert_eq!(by_slug["qwen3.7-max"]["max_context_window"], 1_000_000);
        assert_eq!(by_slug["glm-5.2"]["context_window"], 3_000_000);
        // fallback 行为：base_instructions 内联、其余字段由引擎 serde 默认补齐。
        assert!(
            by_slug["qwen3.7-max"]["base_instructions"]
                .as_str()
                .unwrap()
                .starts_with("You are a coding agent running in the Codex CLI")
        );
    }

    #[test]
    fn catalog_empty_when_no_suffix_models() {
        let catalog = build_model_catalog(&[rm("plain-model")]);
        assert_eq!(catalog["models"].as_array().unwrap().len(), 0);
    }
}
