use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use clap::{Parser, Subcommand};
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind, KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use dirs::home_dir;
use rand::RngCore;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::{self, Write as IoWrite};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(windows)]
use std::os::windows::fs::{symlink_dir, symlink_file};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use url::Url;

mod stats;
mod probe;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const PROVIDER_CONFIG_FILE_NAME: &str = "cx.providers.config.yaml";
const LEGACY_PROVIDER_CONFIG_FILE_NAME: &str = "config.yaml";
const LAUNCH_HOME_DIR_NAME: &str = "cx-launch-homes";
const LAUNCH_HOME_TTL_SECS: u64 = 60 * 60 * 24;
const ADD_PROVIDER_SENTINEL: &str = "+ 添加 Provider";
const ADD_NEW_PROVIDER_SENTINEL: &str = "+ 新建 Provider";
const ADD_WIRE_API_ACTION: &str = "添加 wire_api";
const ADD_MODEL_ACTION: &str = "添加 model";
const DEFAULT_PROVIDER_CONFIG_YAML: &str = include_str!("../config/providers.default.yaml");

// ══════════════════════════════════════════════════
// 配置结构体（从 YAML 反序列化）
// ══════════════════════════════════════════════════

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
struct CxConfig {
    #[serde(default)]
    providers: Vec<ProviderConfig>,
    #[serde(default)]
    agents: Vec<AgentConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct ProviderConfig {
    name: String,
    #[serde(default)]
    apikey_source: Option<String>,
    #[serde(default)]
    models: BTreeMap<String, ProviderModelConfig>,
    #[serde(default)]
    endpoints: BTreeMap<String, ProviderEndpointSpec>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
enum ProviderEndpointSpec {
    Url(String),
    Detailed(ProviderEndpointDetail),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct ProviderEndpointDetail {
    url: String,
    #[serde(default)]
    agents: Vec<String>,
    #[serde(default)]
    copilot_auth: Option<String>,
}

#[derive(Debug, Clone)]
struct EndpointConfig {
    wire_api: String,
    url: String,
    agents: Vec<String>,
    copilot_auth: Option<String>,
    models: Vec<ModelConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct ModelConfig {
    id: String,
    #[serde(default)]
    swe_pro: Option<String>,
    #[serde(default)]
    lcb_pro: Option<String>,
    #[serde(default)]
    hle: Option<String>,
    #[serde(default)]
    desc: Option<String>,
    #[serde(default)]
    context: Option<String>,
    #[serde(default)]
    agents: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
struct ProviderModelConfig {
    #[serde(default)]
    swe_pro: Option<String>,
    #[serde(default)]
    lcb_pro: Option<String>,
    #[serde(default)]
    hle: Option<String>,
    #[serde(default)]
    desc: Option<String>,
    #[serde(default)]
    context: Option<String>,
    #[serde(default)]
    wire_apis: Vec<String>,
    #[serde(default)]
    agents: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct AgentConfig {
    id: String,
    #[serde(alias = "bin")]
    binary: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    wire_apis: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApiKeySourceKind {
    None,
    Env,
    Keychain,
    Literal,
    Shell,
}

impl ApiKeySourceKind {
    fn all() -> [Self; 5] {
        [
            Self::None,
            Self::Env,
            Self::Keychain,
            Self::Literal,
            Self::Shell,
        ]
    }

    fn label(self) -> &'static str {
        match self {
            Self::None => "不设置",
            Self::Env => "env:VAR",
            Self::Keychain => "keychain:SERVICE",
            Self::Literal => "literal:value",
            Self::Shell => "$(shell command)",
        }
    }

    fn prompt(self) -> &'static str {
        match self {
            Self::None => "不设置 apikey_source，保留为空",
            Self::Env => "输入环境变量名，例如 DASHSCOPE_API_KEY",
            Self::Keychain => "输入 Keychain service 名称，例如 DASHSCOPE_API_KEY",
            Self::Literal => "输入固定值，仅建议本地调试使用",
            Self::Shell => "输入 shell 命令内容，不要带 $( )，例如 op read ...",
        }
    }

    fn build(self, value: &str) -> Option<String> {
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

#[derive(Debug, Clone)]
enum AddOperation {
    Provider {
        provider: ProviderConfig,
    },
    Endpoint {
        provider_name: String,
        wire_api: WireApi,
        endpoint: ProviderEndpointSpec,
    },
    Model {
        provider_name: String,
        wire_api: WireApi,
        model_id: String,
        model: ProviderModelConfig,
    },
}

#[derive(Debug, Clone)]
enum AddResult {
    Provider {
        name: String,
    },
    Endpoint {
        provider_name: String,
        wire_api: WireApi,
    },
    Model {
        provider_name: String,
        wire_api: WireApi,
        model_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PromptOutcome<T> {
    Submit(T),
    Back,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextInputAction {
    None,
    Changed,
    Submit,
    Back,
    Cancel,
}

impl ProviderConfig {
    fn normalized_endpoints(&self) -> Vec<EndpointConfig> {
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
                        lcb_pro: model.lcb_pro.clone(),
                        hle: model.hle.clone(),
                        desc: model.desc.clone(),
                        context: model.context.clone(),
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

    fn has_endpoints(&self) -> bool {
        !self.normalized_endpoints().is_empty()
    }
}

// ══════════════════════════════════════════════════
// 运行时数据结构（从 config 构建，TUI 使用）
// ══════════════════════════════════════════════════

#[derive(Debug, Clone)]
struct ResolvedModel {
    id: String,
    swe_pro: String,
    lcb_pro: String,
    hle: String,
    desc: String,
    context: String,
    wire_api: WireApi,
    provider_name: String,
    endpoint_url: String,
    visible_agents: Vec<String>,
    copilot_auth: CopilotAuth,
    env: BTreeMap<String, String>,
}

impl ResolvedModel {
    fn from_config(
        config: &CxConfig,
        provider: &ProviderConfig,
        endpoint: &EndpointConfig,
        model: &ModelConfig,
    ) -> Self {
        Self {
            id: model.id.clone(),
            swe_pro: model.swe_pro.clone().unwrap_or_else(|| "—".to_string()),
            lcb_pro: model.lcb_pro.clone().unwrap_or_else(|| "—".to_string()),
            hle: model.hle.clone().unwrap_or_else(|| "—".to_string()),
            desc: model.desc.clone().unwrap_or_default(),
            context: model.context.clone().unwrap_or_else(|| "—".to_string()),
            wire_api: WireApi::from_str(&endpoint.wire_api),
            provider_name: provider.name.clone(),
            endpoint_url: endpoint.url.clone(),
            visible_agents: effective_agents_for_model(config, provider, endpoint, model),
            copilot_auth: CopilotAuth::from_endpoint(endpoint),
            env: model.env.clone(),
        }
    }

    fn supports_agent(&self, agent_id: &str) -> bool {
        let agent_id = canonical_agent_id(agent_id);
        self.visible_agents
            .iter()
            .any(|a| canonical_agent_id(a) == agent_id)
    }

    fn probe_cache_key(&self) -> String {
        probe_cache_key(
            &self.provider_name,
            &self.endpoint_url,
            self.wire_api,
            &self.id,
        )
    }
}

#[derive(Debug, Clone)]
struct ModelOption {
    selection_key: String,
    id: String,
    swe_pro: String,
    lcb_pro: String,
    hle: String,
    desc: String,
    context: String,
    variants: Vec<ResolvedModel>,
}

impl ModelOption {
    fn from_variants(variants: Vec<ResolvedModel>) -> Self {
        let first = variants
            .first()
            .expect("ModelOption::from_variants requires at least one variant");
        Self {
            selection_key: format!("{}\t{}", first.provider_name, first.id),
            id: first.id.clone(),
            swe_pro: first.swe_pro.clone(),
            lcb_pro: first.lcb_pro.clone(),
            hle: first.hle.clone(),
            desc: first.desc.clone(),
            context: first.context.clone(),
            variants,
        }
    }

    fn default_variant_index(&self) -> usize {
        self.variants
            .iter()
            .enumerate()
            .min_by_key(|(_, variant)| variant.wire_api.priority())
            .map(|(index, _)| index)
            .unwrap_or(0)
    }

    fn selected_variant_index(&self, selected_wire_apis: &BTreeMap<String, usize>) -> usize {
        selected_wire_apis
            .get(&self.selection_key)
            .copied()
            .filter(|index| *index < self.variants.len())
            .unwrap_or_else(|| self.default_variant_index())
    }

    fn selected_variant<'a>(
        &'a self,
        selected_wire_apis: &BTreeMap<String, usize>,
    ) -> &'a ResolvedModel {
        &self.variants[self.selected_variant_index(selected_wire_apis)]
    }

    fn formatted_row(&self, selected_wire_apis: &BTreeMap<String, usize>) -> String {
        let selected = self.selected_variant(selected_wire_apis);
        format!(
            "{:<24} {:>7} {:>7} {:>6}  {:<11} {:>8}  {}",
            self.id,
            self.swe_pro,
            self.lcb_pro,
            self.hle,
            selected.wire_api.display(),
            self.context,
            self.desc
        )
    }
}

fn canonical_agent_id(agent_id: &str) -> &str {
    match agent_id {
        // Backward-compat for legacy config entries only; the CLI no longer proxies `codex app`.
        "codex-app" => "codex",
        _ => agent_id,
    }
}

fn normalize_agent_ids(agent_ids: &[String]) -> Vec<String> {
    let mut normalized = Vec::new();
    for agent_id in agent_ids {
        let canonical = canonical_agent_id(agent_id).to_string();
        if !normalized.iter().any(|existing| existing == &canonical) {
            normalized.push(canonical);
        }
    }
    normalized
}

fn default_wire_apis_for_agent(agent_id: &str) -> Vec<WireApi> {
    match canonical_agent_id(agent_id) {
        "copilot" => vec![WireApi::Anthropic, WireApi::Responses, WireApi::Completions],
        "claude" => vec![WireApi::Anthropic],
        "codex" => vec![WireApi::Responses],
        _ => Vec::new(),
    }
}

fn resolve_agent_wire_apis(agent_id: &str, configured: &[String]) -> Vec<WireApi> {
    let source = if configured.is_empty() {
        default_wire_apis_for_agent(agent_id)
            .into_iter()
            .map(|wire_api| wire_api.display().to_string())
            .collect::<Vec<_>>()
    } else {
        configured.to_vec()
    };

    let mut resolved = Vec::new();
    for item in source {
        let wire_api = WireApi::from_str(&item);
        if wire_api == WireApi::Unavailable || resolved.contains(&wire_api) {
            continue;
        }
        resolved.push(wire_api);
    }
    resolved
}

fn all_compatible_agents(config: &CxConfig, endpoint: &EndpointConfig) -> Vec<String> {
    let wire_api = WireApi::from_str(&endpoint.wire_api);
    resolved_agents(config)
        .into_iter()
        .filter(|agent| agent.supports_wire_api(wire_api))
        .map(|agent| agent.id)
        .collect()
}

fn effective_agents_for_model(
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum WireApi {
    Responses,
    Completions,
    Anthropic,
    Unavailable,
}

impl WireApi {
    fn from_str(s: &str) -> Self {
        match s {
            "responses" => Self::Responses,
            "completions" => Self::Completions,
            "anthropic" => Self::Anthropic,
            _ => Self::Unavailable,
        }
    }

    fn from_cache(value: &str) -> Option<Self> {
        match value {
            "responses" => Some(Self::Responses),
            "completions" => Some(Self::Completions),
            "anthropic" => Some(Self::Anthropic),
            "unavailable" => Some(Self::Unavailable),
            _ => None,
        }
    }

    fn display(self) -> &'static str {
        match self {
            Self::Responses => "responses",
            Self::Completions => "completions",
            Self::Anthropic => "anthropic",
            Self::Unavailable => "unavailable",
        }
    }

    fn priority(self) -> u8 {
        match self {
            Self::Anthropic => 0,
            Self::Responses => 1,
            Self::Completions => 2,
            Self::Unavailable => 3,
        }
    }

    fn launch_value(self) -> Result<&'static str> {
        match self {
            Self::Responses => Ok("responses"),
            Self::Completions => Ok("completions"),
            Self::Anthropic => Ok("anthropic"),
            Self::Unavailable => {
                bail!("该模型当前被标记为 unavailable，请先运行 `cx probe` 更新探测结果")
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CopilotAuth {
    ApiKey,
    BearerToken,
}

fn probe_cache_key(
    provider_name: &str,
    endpoint_url: &str,
    wire_api: WireApi,
    model_id: &str,
) -> String {
    format!(
        "{provider_name}\t{}\t{endpoint_url}\t{model_id}",
        wire_api.display()
    )
}

impl CopilotAuth {
    pub(crate) fn from_endpoint(endpoint: &EndpointConfig) -> Self {
        match endpoint.copilot_auth.as_deref() {
            Some("bearer_token") => Self::BearerToken,
            _ => Self::ApiKey,
        }
    }
}

#[derive(Debug, Clone)]
struct ResolvedProvider {
    name: String,
    has_endpoints: bool,
    apikey_source: Option<String>,
}

impl ResolvedProvider {
    fn from_config(config: &ProviderConfig) -> Self {
        Self {
            name: config.name.clone(),
            has_endpoints: config.has_endpoints(),
            apikey_source: config.apikey_source.clone(),
        }
    }

    fn requires_model(&self) -> bool {
        self.has_endpoints
    }
}

#[derive(Debug, Clone)]
struct Selection {
    agent_id: String,
    agent_binary: String,
    agent_args: Vec<String>,
    agent_env: BTreeMap<String, String>,
    provider: ResolvedProvider,
    model: Option<ResolvedModel>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedAgent {
    id: String,
    binary: String,
    args: Vec<String>,
    supported_wire_apis: Vec<WireApi>,
    env: BTreeMap<String, String>,
}

impl ResolvedAgent {
    fn supports_wire_api(&self, wire_api: WireApi) -> bool {
        self.supported_wire_apis.contains(&wire_api)
    }
}

fn resolved_agents(config: &CxConfig) -> Vec<ResolvedAgent> {
    let mut agents = Vec::new();
    for agent in &config.agents {
        let id = canonical_agent_id(&agent.id).to_string();
        if agents
            .iter()
            .any(|existing: &ResolvedAgent| existing.id == id)
        {
            continue;
        }
        let binary = if id == "codex" {
            "codex".to_string()
        } else {
            agent.binary.clone()
        };
        let supported_wire_apis = resolve_agent_wire_apis(&id, &agent.wire_apis);
        agents.push(ResolvedAgent {
            id,
            binary,
            args: agent.args.clone(),
            supported_wire_apis,
            env: agent.env.clone(),
        });
    }
    agents
}

fn default_agent_configs() -> Vec<AgentConfig> {
    vec![
        AgentConfig {
            id: "copilot".into(),
            binary: "copilot".into(),
            args: Vec::new(),
            wire_apis: vec!["anthropic".into(), "responses".into(), "completions".into()],
            env: BTreeMap::new(),
        },
        AgentConfig {
            id: "claude".into(),
            binary: "claude".into(),
            args: Vec::new(),
            wire_apis: vec!["anthropic".into()],
            env: BTreeMap::new(),
        },
        AgentConfig {
            id: "codex".into(),
            binary: "codex".into(),
            args: Vec::new(),
            wire_apis: vec!["responses".into()],
            env: BTreeMap::new(),
        },
    ]
}

fn available_agents_for_add(config: &CxConfig) -> Vec<ResolvedAgent> {
    let agents = resolved_agents(config);
    if !agents.is_empty() {
        return agents;
    }

    resolved_agents(&CxConfig {
        providers: Vec::new(),
        agents: default_agent_configs(),
    })
}

fn compatible_agents_for_wire_api(config: &CxConfig, wire_api: WireApi) -> Vec<String> {
    available_agents_for_add(config)
        .into_iter()
        .filter(|agent| agent.supports_wire_api(wire_api))
        .map(|agent| agent.id)
        .collect()
}

fn find_agent(config: &CxConfig, agent_id: &str) -> Option<ResolvedAgent> {
    let agent_id = canonical_agent_id(agent_id);
    resolved_agents(config)
        .into_iter()
        .find(|agent| agent.id == agent_id)
}

fn unsupported_codex_app_message() -> &'static str {
    "`cx` 不再代理 `codex app` 桌面版。请直接运行原生 `codex app ...`；终端版仍可继续使用 `cx codex ...`。"
}

// ══════════════════════════════════════════════════
// 配置加载
// ══════════════════════════════════════════════════

fn provider_config_path() -> Result<PathBuf> {
    Ok(cx_state_dir()?.join(PROVIDER_CONFIG_FILE_NAME))
}

fn legacy_provider_config_path() -> Result<PathBuf> {
    Ok(cx_state_dir()?.join(LEGACY_PROVIDER_CONFIG_FILE_NAME))
}

fn migrate_legacy_provider_config(current_path: &Path, legacy_path: &Path) -> Result<bool> {
    if current_path.exists() || !legacy_path.exists() {
        return Ok(false);
    }

    if let Some(parent) = current_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("创建配置目录失败: {}", parent.display()))?;
    }

    fs::rename(legacy_path, current_path).with_context(|| {
        format!(
            "迁移旧配置失败: {} -> {}",
            legacy_path.display(),
            current_path.display()
        )
    })?;
    eprintln!("已将旧 Provider 配置迁移到 {}", current_path.display());
    Ok(true)
}

fn active_provider_config_path() -> Result<PathBuf> {
    let current_path = provider_config_path()?;
    let legacy_path = legacy_provider_config_path()?;
    migrate_legacy_provider_config(&current_path, &legacy_path)?;
    Ok(current_path)
}

fn read_config_file(path: &Path) -> Result<CxConfig> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("读取配置文件失败: {}", path.display()))?;
    serde_yaml::from_str(&content).with_context(|| format!("解析配置文件失败: {}", path.display()))
}

fn create_default_provider_config(path: &Path) -> Result<()> {
    write_string_atomic(path, DEFAULT_PROVIDER_CONFIG_YAML)
        .with_context(|| format!("创建默认 Provider 配置失败: {}", path.display()))?;
    eprintln!("未找到 Provider 配置，已按基线创建: {}", path.display());
    Ok(())
}

fn write_string_atomic(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("创建配置目录失败: {}", parent.display()))?;
    }

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(PROVIDER_CONFIG_FILE_NAME);
    let tmp_path = path.with_file_name(format!(".{file_name}.{}.tmp", random_urlsafe(6)));
    fs::write(&tmp_path, content)
        .with_context(|| format!("写入临时配置文件失败: {}", tmp_path.display()))?;

    if let Err(err) = fs::rename(&tmp_path, path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(err).with_context(|| {
            format!(
                "替换配置文件失败: {} -> {}",
                tmp_path.display(),
                path.display()
            )
        });
    }

    Ok(())
}

fn ensure_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("创建目录失败: {}", path.display()))?;
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("设置目录权限失败: {}", path.display()))?;
    Ok(())
}

fn write_private_file(path: &Path, content: &str) -> Result<()> {
    write_string_atomic(path, content)?;
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("设置文件权限失败: {}", path.display()))?;
    Ok(())
}

#[cfg(unix)]
fn symlink_path(src: &Path, dst: &Path) -> Result<()> {
    std::os::unix::fs::symlink(src, dst)
        .with_context(|| format!("创建符号链接失败: {} -> {}", dst.display(), src.display()))
}

#[cfg(windows)]
fn symlink_path(src: &Path, dst: &Path) -> Result<()> {
    let result = if src.is_dir() {
        symlink_dir(src, dst)
    } else {
        symlink_file(src, dst)
    };
    result.with_context(|| format!("创建符号链接失败: {} -> {}", dst.display(), src.display()))
}

fn launch_homes_root() -> PathBuf {
    env::temp_dir().join(LAUNCH_HOME_DIR_NAME)
}

fn sweep_old_launch_homes() -> Result<()> {
    let root = launch_homes_root();
    if !root.exists() {
        return Ok(());
    }

    let now = current_unix_secs()?;
    for entry in fs::read_dir(&root).with_context(|| format!("读取目录失败: {}", root.display()))?
    {
        let entry = entry.with_context(|| format!("读取目录项失败: {}", root.display()))?;
        let path = entry.path();
        let modified = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs());

        match modified {
            Some(modified) if now.saturating_sub(modified) <= LAUNCH_HOME_TTL_SECS => continue,
            _ => {}
        }

        if path.is_dir() {
            let _ = fs::remove_dir_all(&path);
        } else {
            let _ = fs::remove_file(&path);
        }
    }

    Ok(())
}

fn create_launch_home(agent_id: &str) -> Result<PathBuf> {
    sweep_old_launch_homes()?;
    let root = launch_homes_root();
    ensure_private_dir(&root)?;
    let dir = root.join(format!(
        "{}-{}-{}",
        agent_id,
        current_unix_secs()?,
        random_urlsafe(6)
    ));
    ensure_private_dir(&dir)?;
    Ok(dir)
}

fn mirror_home_entries(real_home: &Path, fake_home: &Path, excluded: &[&str]) -> Result<()> {
    ensure_private_dir(fake_home)?;
    for entry in fs::read_dir(real_home)
        .with_context(|| format!("读取主目录失败: {}", real_home.display()))?
    {
        let entry = entry.with_context(|| format!("读取目录项失败: {}", real_home.display()))?;
        let name = entry.file_name();
        let name_string = name.to_string_lossy();
        if excluded
            .iter()
            .any(|excluded_name| *excluded_name == name_string)
        {
            continue;
        }
        symlink_path(&entry.path(), &fake_home.join(&name))?;
    }
    Ok(())
}

fn materialize_passthrough_dir(
    real_dir: &Path,
    fake_dir: &Path,
    overridden: &[&str],
) -> Result<()> {
    ensure_private_dir(fake_dir)?;
    if !real_dir.exists() {
        return Ok(());
    }

    for entry in
        fs::read_dir(real_dir).with_context(|| format!("读取目录失败: {}", real_dir.display()))?
    {
        let entry = entry.with_context(|| format!("读取目录项失败: {}", real_dir.display()))?;
        let name = entry.file_name();
        let name_string = name.to_string_lossy();
        if overridden
            .iter()
            .any(|overridden_name| *overridden_name == name_string)
        {
            continue;
        }
        symlink_path(&entry.path(), &fake_dir.join(&name))?;
    }
    Ok(())
}

fn toml_basic_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn merge_codex_config(
    existing: Option<&str>,
    model: &ResolvedModel,
    workspace_root: &Path,
) -> Result<String> {
    let project_section = format!(
        "[projects.{}]",
        toml_basic_string(&workspace_root.to_string_lossy())
    );
    let mut retained = Vec::new();
    let mut skipping_section = false;

    if let Some(existing) = existing {
        for line in existing.lines() {
            let trimmed = line.trim();
            if skipping_section {
                if trimmed.starts_with('[') && trimmed.ends_with(']') {
                    skipping_section = false;
                } else {
                    continue;
                }
            }

            if trimmed == "[model_providers.dashscope]" || trimmed == project_section {
                skipping_section = true;
                continue;
            }

            if !trimmed.starts_with('[')
                && (trimmed.starts_with("model =") || trimmed.starts_with("model_provider ="))
            {
                continue;
            }

            retained.push(line.to_string());
        }
    }

    let wire_api = model.wire_api.launch_value()?;
    let mut rendered = format!(
        "model = {}\nmodel_provider = \"dashscope\"\n\n[model_providers.dashscope]\nname = \"DashScope\"\nbase_url = {}\nenv_key = \"DASHSCOPE_API_KEY\"\nwire_api = {}\n\n{}{}\ntrust_level = \"trusted\"\n",
        toml_basic_string(&model.id),
        toml_basic_string(&model.endpoint_url),
        toml_basic_string(wire_api),
        project_section,
        "\n"
    );

    let retained = retained.join("\n").trim().to_string();
    if !retained.is_empty() {
        rendered.push('\n');
        rendered.push_str(&retained);
        rendered.push('\n');
    }

    Ok(rendered)
}

fn prepare_codex_launch_home(
    model: &ResolvedModel,
    env: &mut BTreeMap<String, String>,
) -> Result<()> {
    let real_home = home_dir().context("无法解析用户主目录")?;
    let fake_home = create_launch_home("codex")?;
    mirror_home_entries(&real_home, &fake_home, &[".codex"])?;

    let real_codex_dir = real_home.join(".codex");
    let fake_codex_dir = fake_home.join(".codex");
    materialize_passthrough_dir(&real_codex_dir, &fake_codex_dir, &["config.toml"])?;

    let existing_config = fs::read_to_string(real_codex_dir.join("config.toml")).ok();
    let merged_config =
        merge_codex_config(existing_config.as_deref(), model, &env::current_dir()?)?;
    write_private_file(&fake_codex_dir.join("config.toml"), &merged_config)?;

    env.insert("HOME".into(), fake_home.display().to_string());
    env.insert(
        "XDG_CONFIG_HOME".into(),
        fake_home.join(".config").display().to_string(),
    );
    env.insert("CODEX_HOME".into(), fake_codex_dir.display().to_string());
    Ok(())
}

fn load_config() -> Result<CxConfig> {
    let path = active_provider_config_path()?;
    if !path.exists() {
        create_default_provider_config(&path)?;
    }
    read_config_file(&path)
}

fn load_config_for_add() -> Result<(CxConfig, PathBuf)> {
    let path = active_provider_config_path()?;
    let config = if path.exists() {
        read_config_file(&path)?
    } else {
        CxConfig {
            providers: Vec::new(),
            agents: default_agent_configs(),
        }
    };
    Ok((config, path))
}

fn save_config(path: &Path, config: &CxConfig) -> Result<()> {
    let yaml = serde_yaml::to_string(config).context("序列化配置失败")?;
    write_string_atomic(path, &yaml)
}

fn resolve_apikey(source: &str) -> Result<String> {
    if let Some(rest) = source.strip_prefix("keychain:") {
        keychain_secret(rest)
    } else if let Some(rest) = source.strip_prefix("env:") {
        env::var(rest).with_context(|| format!("环境变量 `{rest}` 未设置"))
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

fn cx_state_dir() -> Result<PathBuf> {
    let home = home_dir().context("无法解析用户主目录")?;
    Ok(home.join(".config/cx"))
}

fn current_unix_secs() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("系统时间早于 Unix Epoch")?
        .as_secs())
}

fn random_urlsafe(bytes: usize) -> String {
    let mut raw = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut raw);
    URL_SAFE_NO_PAD.encode(raw)
}

fn build_all_models(config: &CxConfig) -> Vec<ResolvedModel> {
    let mut models = Vec::new();
    for provider in &config.providers {
        for endpoint in provider.normalized_endpoints() {
            for model in &endpoint.models {
                models.push(ResolvedModel::from_config(
                    config, provider, &endpoint, model,
                ));
            }
        }
    }
    models
}

fn provider_supports_agent(config: &CxConfig, provider: &ProviderConfig, agent_id: &str) -> bool {
    let agent_id = canonical_agent_id(agent_id);
    if !provider.has_endpoints() {
        return true;
    }

    provider.normalized_endpoints().iter().any(|endpoint| {
        endpoint.models.iter().any(|model| {
            effective_agents_for_model(config, provider, endpoint, model)
                .iter()
                .any(|candidate| canonical_agent_id(candidate) == agent_id)
        })
    })
}

fn apply_probe_cache(models: &mut [ResolvedModel]) {
    if let Ok(cache) = load_probe_cache() {
        for model in models.iter_mut() {
            if let Some(wire_api) = cache
                .models
                .get(&model.probe_cache_key())
                .and_then(|value| WireApi::from_cache(value))
            {
                model.wire_api = wire_api;
            }
        }
    }
}

fn providers_for_agent(config: &CxConfig, agent_id: &str) -> Vec<ResolvedProvider> {
    let agent_id = canonical_agent_id(agent_id);
    let mut providers: Vec<ResolvedProvider> = config
        .providers
        .iter()
        .filter(|provider| provider_supports_agent(config, provider, agent_id))
        .map(ResolvedProvider::from_config)
        .collect();

    // Append the "add provider" sentinel
    providers.push(ResolvedProvider {
        name: ADD_PROVIDER_SENTINEL.to_string(),
        has_endpoints: false,
        apikey_source: None,
    });
    providers
}

fn model_options_for_provider(
    all_models: &[ResolvedModel],
    agent_id: &str,
    provider_name: &str,
) -> Vec<ModelOption> {
    let mut grouped: Vec<Vec<ResolvedModel>> = Vec::new();
    let mut indexes_by_id: BTreeMap<String, usize> = BTreeMap::new();

    for model in all_models
        .iter()
        .filter(|m| m.provider_name == provider_name && m.supports_agent(agent_id))
    {
        if let Some(index) = indexes_by_id.get(&model.id).copied() {
            grouped[index].push(model.clone());
            continue;
        }

        indexes_by_id.insert(model.id.clone(), grouped.len());
        grouped.push(vec![model.clone()]);
    }

    grouped
        .into_iter()
        .map(ModelOption::from_variants)
        .collect()
}

// ══════════════════════════════════════════════════
// CLI definition（clap derive）
// ══════════════════════════════════════════════════

#[derive(Parser)]
#[command(
    name = "cx",
    about = "统一 Agent 入口",
    version = VERSION,
    disable_help_subcommand = true,
    disable_version_flag = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<CxCommand>,
}

#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
enum CxCommand {
    /// 显示帮助信息
    Help,
    /// 通过向导交互式新增 Provider / wire_api / model
    Add,
    /// 探测模型的 completions / responses 支持情况
    Probe {
        /// 筛选要显示的 providers（逗号分隔）
        #[arg(long, value_name = "PROVIDERS")]
        provider: Option<String>,
        /// 自动探测并退出（不启动 TUI）
        #[arg(long)]
        auto_probe: bool,
    },
    /// 从 URL 或本地文件读取 Provider 配置并合并到本地
    Patch {
        /// 本地 YAML 文件路径，或远程配置 URL
        #[arg(value_name = "SOURCE", conflicts_with_all = ["url", "refresh"])]
        source: Option<String>,
        /// 远程配置 YAML 文件的 URL
        #[arg(long, conflicts_with = "source")]
        url: Option<String>,
        /// 使用上次记录的 URL 重新获取配置
        #[arg(long, conflicts_with_all = ["source", "url"])]
        refresh: bool,
    },
    /// 查看各 agent × model 的 token 用量统计（TUI）
    Stats,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DispatchCommand {
    Help,
    Add,
    Probe {
        provider: Option<String>,
        auto_probe: bool,
    },
    Patch {
        source: Option<String>,
        url: Option<String>,
        refresh: bool,
    },
    Stats,
    Launch {
        args: Vec<String>,
    },
}

fn dispatch_command(raw_args: &[String]) -> DispatchCommand {
    match Cli::try_parse_from(raw_args) {
        Ok(cli) => match cli.command {
            Some(CxCommand::Help) => DispatchCommand::Help,
            Some(CxCommand::Add) => DispatchCommand::Add,
            Some(CxCommand::Patch {
                source,
                url,
                refresh,
            }) => DispatchCommand::Patch {
                source,
                url,
                refresh,
            },
            Some(CxCommand::Probe { provider, auto_probe }) => DispatchCommand::Probe { provider, auto_probe },
            Some(CxCommand::Stats) => DispatchCommand::Stats,
            None => DispatchCommand::Launch { args: Vec::new() },
        },
        Err(_) => DispatchCommand::Launch {
            args: raw_args[1..].to_vec(),
        },
    }
}

// ══════════════════════════════════════════════════
// 入口
// ══════════════════════════════════════════════════

pub fn run() -> Result<()> {
    let raw_args: Vec<String> = env::args().collect();
    match dispatch_command(&raw_args) {
        DispatchCommand::Help => {
            print_help();
            Ok(())
        }
        DispatchCommand::Add => {
            run_add()?;
            Ok(())
        }
        DispatchCommand::Patch {
            source,
            url,
            refresh,
        } => run_patch(source, url, refresh),
        DispatchCommand::Probe { provider, auto_probe } => {
            let config = load_config()?;
            run_probe(provider, auto_probe, &config)
        }
        DispatchCommand::Stats => stats::run_stats(),
        DispatchCommand::Launch { args } => {
            // No subcommand or an unknown one → treat as Launch with optional agent hint.
            let config = load_config()?;

            if let Some(first) = args.first() {
                if first == "codex-app"
                    || (first == "codex" && args.get(1).map(String::as_str) == Some("app"))
                {
                    bail!(unsupported_codex_app_message());
                }
                if let Some(agent) = find_agent(&config, first) {
                    return run_launcher(Some(agent.id), &config, args[1..].to_vec());
                }
            }

            run_launcher(None, &config, args)
        }
    }
}

// ══════════════════════════════════════════════════
// Launcher
// ══════════════════════════════════════════════════

fn run_launcher(
    agent_hint: Option<String>,
    config: &CxConfig,
    passthrough_args: Vec<String>,
) -> Result<()> {
    let rerun_agent_hint = agent_hint.clone();
    let mut all_models = build_all_models(config);
    apply_probe_cache(&mut all_models);

    let selection = run_tui(agent_hint, config, &all_models)?;

    let Some(selection) = selection else {
        return Ok(());
    };

    if selection.provider.name == ADD_PROVIDER_SENTINEL {
        if run_add()? {
            let refreshed = load_config()?;
            return run_launcher(rerun_agent_hint, &refreshed, passthrough_args);
        }
        return Ok(());
    }

    let spec = build_launch_spec(&selection, &passthrough_args)?;

    println!();
    println!("{}", spec.summary);
    println!();

    apply_selected_model_tab_name(&selection)?;
    exec_launch(spec)
}
fn apply_selected_model_tab_name(selection: &Selection) -> Result<()> {
    let Some(model_id) = selection
        .model
        .as_ref()
        .map(|model| sanitize_terminal_title(&model.id))
    else {
        return Ok(());
    };

    if model_id.is_empty() {
        return Ok(());
    }

    let mut stdout = io::stdout();
    write!(stdout, "\x1b]1;{model_id}\x07\x1b]2;{model_id}\x07")
        .context("设置终端 tab 名称失败")?;
    stdout.flush().context("刷新终端 title 失败")?;
    Ok(())
}

fn sanitize_terminal_title(title: &str) -> String {
    title.chars().filter(|ch| !ch.is_ascii_control()).collect()
}

fn print_help() {
    Cli::parse_from(["cx", "--help"]);
}

// ══════════════════════════════════════════════════
// Patch — 远程 Provider 配置合并
// ══════════════════════════════════════════════════

fn patch_source_path() -> Result<PathBuf> {
    let home = home_dir().context("无法解析用户主目录")?;
    Ok(home.join(".config/cx/.patch_source"))
}

fn save_patch_source(url: &str) -> Result<()> {
    let path = patch_source_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("创建配置目录失败: {}", parent.display()))?;
    }
    fs::write(&path, url).with_context(|| "保存 patch 来源 URL 失败")?;
    Ok(())
}

fn load_patch_source() -> Result<String> {
    let path = patch_source_path()?;
    fs::read_to_string(&path)
        .map(|s| s.trim().to_string())
        .with_context(|| {
            format!(
                "未找到 patch 来源 URL，请先运行 `cx patch --url <url>`: {}",
                path.display()
            )
        })
}

enum PatchInput {
    Remote(String),
    Local(PathBuf),
}

fn patch_input_from_source(source: &str) -> PatchInput {
    match Url::parse(source) {
        Ok(url) if matches!(url.scheme(), "http" | "https") => PatchInput::Remote(source.into()),
        _ => PatchInput::Local(PathBuf::from(source)),
    }
}

fn run_patch(source: Option<String>, url: Option<String>, refresh: bool) -> Result<()> {
    let runtime = tokio::runtime::Runtime::new().context("初始化 tokio runtime 失败")?;
    runtime.block_on(async_run_patch(source, url, refresh))
}

async fn async_run_patch(source: Option<String>, url: Option<String>, refresh: bool) -> Result<()> {
    let input = if refresh {
        PatchInput::Remote(load_patch_source()?)
    } else if let Some(source) = url.or(source) {
        patch_input_from_source(&source)
    } else {
        bail!("请指定 <path-or-url>、--url <url> 或 --refresh")
    };

    let body = match &input {
        PatchInput::Remote(url) => {
            println!("从 {} 下载 Provider 配置...", url);

            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .context("初始化 HTTP 客户端失败")?;

            let response = client
                .get(url)
                .send()
                .await
                .with_context(|| format!("下载配置失败: {url}"))?;

            let status = response.status();
            if !status.is_success() {
                bail!("下载配置失败: HTTP {}", status.as_u16());
            }

            response.text().await.context("读取响应失败")?
        }
        PatchInput::Local(path) => {
            println!("从 {} 读取 Provider 配置...", path.display());
            fs::read_to_string(path)
                .with_context(|| format!("读取本地配置失败: {}", path.display()))?
        }
    };
    let incoming: CxConfig =
        serde_yaml::from_str(&body).with_context(|| "解析 Provider 配置失败")?;

    let config_path = active_provider_config_path()?;
    let existing = if config_path.exists() {
        read_config_file(&config_path)?
    } else {
        CxConfig::default()
    };

    let merged = CxConfig {
        providers: merge_providers(&existing.providers, &incoming.providers),
        agents: merge_agents(&existing.agents, &incoming.agents),
    };

    let yaml = serde_yaml::to_string(&merged).context("序列化配置失败")?;
    write_string_atomic(&config_path, &yaml)?;

    println!("配置已更新: {}", config_path.display());
    if let PatchInput::Remote(url) = &input {
        save_patch_source(url)?;
        println!("来源 URL 已记录，后续可通过 `cx patch --refresh` 更新。");
    }

    Ok(())
}

/// Replace providers by `name`; new providers are appended.
/// Preserves existing order; incoming replacements stay in-place, new items are appended at end.
fn merge_providers(
    existing: &[ProviderConfig],
    incoming: &[ProviderConfig],
) -> Vec<ProviderConfig> {
    let mut replaced = vec![false; incoming.len()];
    let mut result = Vec::with_capacity(existing.len() + incoming.len());

    for existing_provider in existing {
        if let Some((index, replacement)) = incoming
            .iter()
            .enumerate()
            .find(|(_, provider)| provider.name == existing_provider.name)
        {
            replaced[index] = true;
            result.push(replacement.clone());
        } else {
            result.push(existing_provider.clone());
        }
    }

    for (index, provider) in incoming.iter().enumerate() {
        if !replaced[index] {
            result.push(provider.clone());
        }
    }

    result
}

/// Replace agents by `id`; new agents are appended.
/// Preserves existing order; incoming replacements stay in-place, new items are appended at end.
fn merge_agents(existing: &[AgentConfig], incoming: &[AgentConfig]) -> Vec<AgentConfig> {
    let mut replaced = vec![false; incoming.len()];
    let mut result = Vec::with_capacity(existing.len() + incoming.len());

    for existing_agent in existing {
        if let Some((index, replacement)) = incoming
            .iter()
            .enumerate()
            .find(|(_, agent)| agent.id == existing_agent.id)
        {
            replaced[index] = true;
            result.push(replacement.clone());
        } else {
            result.push(existing_agent.clone());
        }
    }

    for (index, agent) in incoming.iter().enumerate() {
        if !replaced[index] {
            result.push(agent.clone());
        }
    }

    result
}

fn validate_provider_name(config: &CxConfig, name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        bail!("Provider 名称不能为空");
    }
    if name.starts_with("+ ") || name == ADD_PROVIDER_SENTINEL || name == ADD_NEW_PROVIDER_SENTINEL
    {
        bail!("Provider 名称不能使用保留的向导项");
    }
    if config
        .providers
        .iter()
        .any(|provider| provider.name == name)
    {
        bail!("Provider `{name}` 已存在");
    }
    Ok(name.to_string())
}

fn validate_required_text(value: &str, label: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() {
        bail!("{label} 不能为空");
    }
    Ok(value.to_string())
}

fn validate_endpoint_url(url: &str) -> Result<String> {
    let url = validate_required_text(url, "URL")?;
    let parsed = Url::parse(&url).context("URL 不是合法地址")?;
    match parsed.scheme() {
        "http" | "https" => Ok(url),
        other => bail!("URL 必须使用 http/https 协议，当前为 `{other}`"),
    }
}

fn validate_apikey_payload(kind: ApiKeySourceKind, value: &str) -> Result<String> {
    let value = validate_required_text(value, "apikey_source 内容")?;
    if kind == ApiKeySourceKind::Shell && value.contains("$(") {
        bail!("shell 命令输入时不要再包含 `$(` 或 `)`，只填命令内容即可");
    }
    Ok(value)
}

fn validate_model_id(provider: &ProviderConfig, model_id: &str) -> Result<String> {
    let model_id = validate_required_text(model_id, "Model ID")?;
    if provider.models.contains_key(&model_id) {
        bail!(
            "Provider `{}` 已存在 model `{model_id}`；当前配置以 model id 作为 key，不能重复创建",
            provider.name
        );
    }
    Ok(model_id)
}

fn provider_by_name_mut<'a>(
    config: &'a mut CxConfig,
    provider_name: &str,
) -> Result<&'a mut ProviderConfig> {
    config
        .providers
        .iter_mut()
        .find(|provider| provider.name == provider_name)
        .with_context(|| format!("找不到 Provider `{provider_name}`"))
}

fn apply_add_operation(config: &mut CxConfig, operation: AddOperation) -> Result<AddResult> {
    match operation {
        AddOperation::Provider { provider } => {
            let name = validate_provider_name(config, &provider.name)?;
            config.providers.push(provider);
            Ok(AddResult::Provider { name })
        }
        AddOperation::Endpoint {
            provider_name,
            wire_api,
            endpoint,
        } => {
            let provider = provider_by_name_mut(config, &provider_name)?;
            let wire_api_key = wire_api.display().to_string();
            if provider.endpoints.contains_key(&wire_api_key) {
                bail!(
                    "Provider `{}` 已存在 `{}` endpoint",
                    provider.name,
                    wire_api.display()
                );
            }
            provider.endpoints.insert(wire_api_key, endpoint);
            Ok(AddResult::Endpoint {
                provider_name,
                wire_api,
            })
        }
        AddOperation::Model {
            provider_name,
            wire_api,
            model_id,
            model,
        } => {
            let provider = provider_by_name_mut(config, &provider_name)?;
            if !provider.endpoints.contains_key(wire_api.display()) {
                bail!(
                    "Provider `{}` 缺少 `{}` endpoint，请先添加 wire_api",
                    provider.name,
                    wire_api.display()
                );
            }
            let model_id = validate_model_id(provider, &model_id)?;
            provider.models.insert(model_id.clone(), model);
            Ok(AddResult::Model {
                provider_name,
                wire_api,
                model_id,
            })
        }
    }
}

fn add_result_message(result: &AddResult) -> String {
    match result {
        AddResult::Provider { name } => format!("已新增 Provider `{name}`"),
        AddResult::Endpoint {
            provider_name,
            wire_api,
        } => format!(
            "已为 Provider `{provider_name}` 新增 `{}` endpoint",
            wire_api.display()
        ),
        AddResult::Model {
            provider_name,
            wire_api,
            model_id,
        } => format!(
            "已为 Provider `{provider_name}` 的 `{}` endpoint 新增 model `{model_id}`",
            wire_api.display()
        ),
    }
}

fn add_operation_preview(operation: &AddOperation) -> Result<String> {
    match operation {
        AddOperation::Provider { provider } => {
            serde_yaml::to_string(provider).context("生成 Provider 预览失败")
        }
        AddOperation::Endpoint {
            wire_api, endpoint, ..
        } => {
            let mut endpoints = BTreeMap::new();
            endpoints.insert(wire_api.display().to_string(), endpoint.clone());
            serde_yaml::to_string(&endpoints).context("生成 endpoint 预览失败")
        }
        AddOperation::Model {
            model_id, model, ..
        } => {
            let mut models = BTreeMap::new();
            models.insert(model_id.clone(), model.clone());
            serde_yaml::to_string(&models).context("生成 model 预览失败")
        }
    }
}

// ══════════════════════════════════════════════════
// Secret Prompting — 交互式补齐缺失的 API Key
// ══════════════════════════════════════════════════

fn resolve_apikey_interactive(source: &str) -> Result<String> {
    match resolve_apikey(source) {
        Ok(key) => Ok(key),
        Err(e) => {
            if let Some(service) = source.strip_prefix("keychain:") {
                if !cfg!(target_os = "macos") {
                    bail!("`keychain:` 仅支持 macOS Keychain，请改用 `env:` 或在 macOS 上运行。");
                }
                eprintln!("从 Keychain 读取 `{service}` 失败: {e}");
                eprint!("请输入 {service} 的 API Key: ");

                let mut input = String::new();
                io::stdin().read_line(&mut input).context("读取输入失败")?;
                let key = input.trim().to_string();

                if key.is_empty() {
                    bail!("API Key 不能为空");
                }

                let user = env::var("USER").unwrap_or_default();
                let child = Command::new("security")
                    .args([
                        "add-generic-password",
                        "-a",
                        &user,
                        "-s",
                        service,
                        "-w",
                        &key,
                        "-U",
                    ])
                    .spawn()
                    .with_context(|| "写入 Keychain 失败")?;

                let output = child
                    .wait_with_output()
                    .with_context(|| "等待 Keychain 写入完成失败")?;

                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    bail!("写入 Keychain 失败: {}", stderr.trim());
                }

                eprintln!("已将 API Key 保存到 Keychain `{service}`。");
                Ok(key)
            } else if let Some(var) = source.strip_prefix("env:") {
                bail!("环境变量 `{var}` 未设置，请通过 `export {var}=<your-key>` 设置后重试")
            } else {
                Err(e)
            }
        }
    }
}

fn exec_launch(spec: LaunchSpec) -> Result<()> {
    let mut command = Command::new(&spec.program);
    command.args(&spec.args);
    for name in &spec.env_remove {
        command.env_remove(name);
    }
    command.envs(&spec.env);

    if spec.detach {
        // GUI app: spawn in background, then exit terminal
        #[cfg(unix)]
        {
            command.process_group(0); // separate process group
            let _child = command
                .spawn()
                .with_context(|| format!("启动 `{}` 失败", spec.program.display()))?;
            // Don't wait — just exit
            return Ok(());
        }
        #[cfg(not(unix))]
        {
            let _child = command
                .spawn()
                .with_context(|| format!("启动 `{}` 失败", spec.program.display()))?;
            return Ok(());
        }
    }

    // CLI: exec replaces current process
    #[cfg(unix)]
    {
        let error = command.exec();
        Err(anyhow!("启动 `{}` 失败: {error}", spec.program.display()))
    }

    #[cfg(not(unix))]
    {
        let status = command
            .status()
            .with_context(|| format!("启动 `{}` 失败", spec.program.display()))?;
        std::process::exit(status.code().unwrap_or(1));
    }
}

#[derive(Debug)]
struct LaunchSpec {
    program: PathBuf,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    summary: String,
    detach: bool,
    env_remove: Vec<String>,
}

fn build_launch_spec(selection: &Selection, passthrough_args: &[String]) -> Result<LaunchSpec> {
    let program = resolve_binary(&selection.agent_binary)?;
    let mut args = Vec::new();
    args.extend(selection.agent_args.iter().cloned());
    let mut env = BTreeMap::new();

    let agent_id = &selection.agent_id;
    let provider = &selection.provider;
    let mut env_remove = Vec::new();

    // Default provider (no endpoints) — use agent's own default behavior
    if !provider.has_endpoints {
        match agent_id.as_str() {
            "copilot" => {
                args.extend(passthrough_args.iter().cloned());
            }
            "claude" => {
                env_remove.push("ANTHROPIC_API_KEY".into());
                env_remove.push("ANTHROPIC_AUTH_TOKEN".into());
                env_remove.push("ANTHROPIC_BASE_URL".into());
                env_remove.push("ANTHROPIC_MODEL".into());
                if let Some(ref source) = provider.apikey_source {
                    let key = resolve_apikey_interactive(source)?;
                    env.insert("ANTHROPIC_API_KEY".into(), key.clone());
                    env.insert("ANTHROPIC_AUTH_TOKEN".into(), key);
                }
                args.extend(passthrough_args.iter().cloned());
            }
            "codex" => {
                if let Some(value) = provider
                    .apikey_source
                    .as_ref()
                    .and_then(|source| resolve_apikey_interactive(source).ok())
                {
                    env.insert("AZURE_OPENAI_API_KEY".into(), value);
                }
                args.extend(passthrough_args.iter().cloned());
            }
            _ => {
                args.extend(passthrough_args.iter().cloned());
            }
        }
    } else {
        // Provider with endpoints — inject env/args based on wire_api and config
        let model = selection.model.as_ref().context(format!(
            "{} 选择了 {}，但没有选中模型",
            agent_id, provider.name
        ))?;

        // Inject a unified model identifier so agents and their tooling can
        // detect which model cx configured, regardless of the agent type.
        env.insert("CX_MODEL".into(), model.id.clone());

        let apikey = if let Some(ref source) = provider.apikey_source {
            resolve_apikey_interactive(source)?
        } else {
            bail!(
                "Provider `{}` 需要 API Key 但未配置 apikey_source",
                provider.name
            );
        };

        match agent_id.as_str() {
            "copilot" => {
                env.insert(
                    "COPILOT_PROVIDER_BASE_URL".into(),
                    model.endpoint_url.clone(),
                );
                env.insert("COPILOT_MODEL".into(), model.id.clone());
                configure_copilot_auth(&mut env, model.copilot_auth, apikey);
                match model.wire_api {
                    WireApi::Anthropic => {
                        env.insert("COPILOT_PROVIDER_TYPE".into(), "anthropic".into());
                    }
                    WireApi::Responses | WireApi::Completions => {
                        env.insert("COPILOT_PROVIDER_TYPE".into(), "openai".into());
                        env.insert(
                            "COPILOT_PROVIDER_WIRE_API".into(),
                            model.wire_api.launch_value()?.to_string(),
                        );
                    }
                    WireApi::Unavailable => {
                        bail!(
                            "`copilot` 当前无法使用 `{}`，因为它被标记为 unavailable。",
                            model.id
                        );
                    }
                }
                args.extend(passthrough_args.iter().cloned());
            }
            "claude" => {
                env_remove.push("ANTHROPIC_API_KEY".into());
                env_remove.push("ANTHROPIC_AUTH_TOKEN".into());
                env_remove.push("ANTHROPIC_BASE_URL".into());
                env_remove.push("ANTHROPIC_MODEL".into());
                env.insert("ANTHROPIC_BASE_URL".into(), model.endpoint_url.clone());
                env.insert("ANTHROPIC_API_KEY".into(), apikey);
                env.insert("ANTHROPIC_MODEL".into(), model.id.clone());
                args.push("--model".into());
                args.push(model.id.clone());
                args.extend(passthrough_args.iter().cloned());
            }
            "codex" => {
                if model.wire_api != WireApi::Responses {
                    bail!(
                        "`codex` 当前仅支持 responses endpoint 模型；`{}` 在内嵌配置中标记为 {}。",
                        model.id,
                        model.wire_api.display()
                    );
                }
                env.insert("DASHSCOPE_API_KEY".into(), apikey);
                prepare_codex_launch_home(model, &mut env)?;
                args.extend(passthrough_args.iter().cloned());
            }
            _ => {
                // Generic fallback: just pass through
                args.extend(passthrough_args.iter().cloned());
            }
        }
    }

    // Agent 级别环境变量
    env.extend(selection.agent_env.iter().map(|(k, v)| (k.clone(), v.clone())));

    // Model 级别环境变量（覆盖 agent 同名变量）
    if let Some(ref model) = selection.model {
        env.extend(model.env.iter().map(|(k, v)| (k.clone(), v.clone())));
    }

    let summary = match &selection.model {
        Some(model) => format!(
            "启动 {} | Provider: {} | Model: {}",
            agent_id, provider.name, model.id
        ),
        None => format!("启动 {} | Provider: {}", agent_id, provider.name),
    };

    Ok(LaunchSpec {
        program,
        args,
        env,
        summary,
        detach: false,
        env_remove,
    })
}

fn configure_copilot_auth(
    env: &mut BTreeMap<String, String>,
    auth: CopilotAuth,
    credential: String,
) {
    match auth {
        CopilotAuth::ApiKey => {
            env.insert("COPILOT_PROVIDER_API_KEY".into(), credential);
        }
        CopilotAuth::BearerToken => {
            env.insert("COPILOT_PROVIDER_BEARER_TOKEN".into(), credential);
        }
    }
}

fn resolve_binary(name: &str) -> Result<PathBuf> {
    // CLI binary: use which + fallback paths
    if let Ok(path) = which::which(name) {
        return Ok(path);
    }

    let home = home_dir().context("无法解析用户主目录")?;
    let fallbacks = [
        home.join(".nvm/versions/node/v20.19.4/bin").join(name),
        home.join(".local/bin").join(name),
        PathBuf::from("/opt/homebrew/bin").join(name),
    ];

    fallbacks
        .into_iter()
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| anyhow!("找不到原生可执行文件 `{name}`"))
}

fn keychain_secret(service: &str) -> Result<String> {
    if !cfg!(target_os = "macos") {
        bail!("`keychain:` 仅支持 macOS Keychain，请改用 `env:` 配置 `{service}`。");
    }

    let user = env::var("USER").unwrap_or_default();
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

    let secret = String::from_utf8(output.stdout)
        .with_context(|| format!("Keychain 返回的 `{service}` 不是合法 UTF-8"))?
        .trim()
        .to_string();

    if secret.is_empty() {
        bail!("Keychain 中的 `{service}` 为空");
    }

    Ok(secret)
}

// ══════════════════════════════════════════════════
// Probe
// ══════════════════════════════════════════════════

#[derive(Debug, Deserialize, Serialize, Default)]
struct ProbeCache {
    timestamp: String,
    models: BTreeMap<String, String>,
}

fn probe_cache_path() -> Result<PathBuf> {
    let home = home_dir().context("无法解析用户主目录")?;
    Ok(home.join(".config/cx/probe_cache.json"))
}

fn load_probe_cache() -> Result<ProbeCache> {
    let path = probe_cache_path()?;
    let content = fs::read_to_string(&path)
        .with_context(|| format!("读取 probe 缓存失败: {}", path.display()))?;
    let cache = serde_json::from_str::<ProbeCache>(&content)
        .with_context(|| format!("解析 probe 缓存失败: {}", path.display()))?;
    Ok(cache)
}

fn run_probe(provider: Option<String>, auto_probe: bool, config: &CxConfig) -> Result<()> {
    if auto_probe {
        probe::run_probe_auto(config, provider)
    } else {
        probe::run_probe_tui(config, provider)
    }
}

// ══════════════════════════════════════════════════
// Add Wizard
// ══════════════════════════════════════════════════

type AppTerminal = Terminal<CrosstermBackend<io::Stdout>>;

fn run_add() -> Result<bool> {
    let (mut config, config_path) = load_config_for_add()?;
    let operation =
        with_terminal_session(|terminal| collect_add_operation(terminal, &config, &config_path))?;
    let Some(operation) = operation else {
        return Ok(false);
    };

    let result = apply_add_operation(&mut config, operation)?;
    save_config(&config_path, &config)?;
    println!("{}", add_result_message(&result));
    println!("配置已更新: {}", config_path.display());
    Ok(true)
}

fn with_terminal_session<T, F>(f: F) -> Result<T>
where
    F: FnOnce(&mut AppTerminal) -> Result<T>,
{
    enable_raw_mode().context("启用终端 raw mode 失败")?;
    let _terminal_guard = TerminalGuard;

    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste).context("进入备用屏幕失败")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("初始化终端失败")?;
    f(&mut terminal)
}

fn collect_add_operation(
    terminal: &mut AppTerminal,
    config: &CxConfig,
    config_path: &Path,
) -> Result<Option<AddOperation>> {
    loop {
        let mut items = config
            .providers
            .iter()
            .map(|provider| provider.name.clone())
            .collect::<Vec<_>>();
        items.push(ADD_NEW_PROVIDER_SENTINEL.to_string());

        match prompt_select(
            terminal,
            "cx add",
            "选择现有 Provider 继续添加，或新建一个 Provider",
            &items,
            0,
            "↑/↓ 或 j/k 移动  ·  Enter 确认  ·  Esc 返回  ·  q 退出",
        )? {
            PromptOutcome::Submit(index) if index < config.providers.len() => {
                match collect_existing_provider_operation(terminal, config, index, config_path)? {
                    PromptOutcome::Submit(operation) => return Ok(Some(operation)),
                    PromptOutcome::Back => continue,
                    PromptOutcome::Cancel => return Ok(None),
                }
            }
            PromptOutcome::Submit(_) => {
                match collect_new_provider_operation(terminal, config, config_path)? {
                    PromptOutcome::Submit(operation) => return Ok(Some(operation)),
                    PromptOutcome::Back => continue,
                    PromptOutcome::Cancel => return Ok(None),
                }
            }
            PromptOutcome::Back | PromptOutcome::Cancel => return Ok(None),
        }
    }
}

fn collect_existing_provider_operation(
    terminal: &mut AppTerminal,
    config: &CxConfig,
    provider_index: usize,
    config_path: &Path,
) -> Result<PromptOutcome<AddOperation>> {
    let provider = &config.providers[provider_index];

    loop {
        let items = vec![
            ADD_WIRE_API_ACTION.to_string(),
            ADD_MODEL_ACTION.to_string(),
        ];
        match prompt_select(
            terminal,
            "cx add",
            &format!("Provider: {} — 选择要执行的新增操作", provider.name),
            &items,
            0,
            "↑/↓ 或 j/k 移动  ·  Enter 确认  ·  Esc 返回  ·  q 退出",
        )? {
            PromptOutcome::Submit(0) => {
                match collect_endpoint_operation(terminal, provider, config_path)? {
                    PromptOutcome::Submit(operation) => {
                        return Ok(PromptOutcome::Submit(operation));
                    }
                    PromptOutcome::Back => continue,
                    PromptOutcome::Cancel => return Ok(PromptOutcome::Cancel),
                }
            }
            PromptOutcome::Submit(1) => {
                match collect_model_operation(terminal, config, provider, config_path)? {
                    PromptOutcome::Submit(operation) => {
                        return Ok(PromptOutcome::Submit(operation));
                    }
                    PromptOutcome::Back => continue,
                    PromptOutcome::Cancel => return Ok(PromptOutcome::Cancel),
                }
            }
            PromptOutcome::Back => return Ok(PromptOutcome::Back),
            PromptOutcome::Cancel => return Ok(PromptOutcome::Cancel),
            PromptOutcome::Submit(_) => unreachable!(),
        }
    }
}

fn collect_new_provider_operation(
    terminal: &mut AppTerminal,
    config: &CxConfig,
    config_path: &Path,
) -> Result<PromptOutcome<AddOperation>> {
    let provider_name = match prompt_text(
        terminal,
        "cx add",
        "输入新的 Provider 名称",
        "",
        "示例：百炼 / Packy API / Xiaomi MIMO",
        |value| validate_provider_name(config, value),
    )? {
        PromptOutcome::Submit(value) => value,
        PromptOutcome::Back => return Ok(PromptOutcome::Back),
        PromptOutcome::Cancel => return Ok(PromptOutcome::Cancel),
    };

    let apikey_kind = match prompt_select(
        terminal,
        "cx add",
        "选择 apikey_source 类型",
        &ApiKeySourceKind::all()
            .into_iter()
            .map(|kind| kind.label().to_string())
            .collect::<Vec<_>>(),
        0,
        "↑/↓ 或 j/k 移动  ·  Enter 确认  ·  Esc 返回  ·  q 退出",
    )? {
        PromptOutcome::Submit(index) => ApiKeySourceKind::all()[index],
        PromptOutcome::Back => return Ok(PromptOutcome::Back),
        PromptOutcome::Cancel => return Ok(PromptOutcome::Cancel),
    };

    let apikey_source = if apikey_kind == ApiKeySourceKind::None {
        None
    } else {
        match prompt_text(
            terminal,
            "cx add",
            apikey_kind.prompt(),
            "",
            "cx 会自动拼接为合法的 apikey_source",
            |value| validate_apikey_payload(apikey_kind, value),
        )? {
            PromptOutcome::Submit(value) => apikey_kind.build(&value),
            PromptOutcome::Back => return Ok(PromptOutcome::Back),
            PromptOutcome::Cancel => return Ok(PromptOutcome::Cancel),
        }
    };

    let endpoints = match prompt_provider_endpoint_form(
        terminal,
        "cx add",
        &format!("为 `{}` 填写支持的 wire_api endpoint URL", provider_name),
    )? {
        PromptOutcome::Submit(endpoints) => endpoints,
        PromptOutcome::Back => return Ok(PromptOutcome::Back),
        PromptOutcome::Cancel => return Ok(PromptOutcome::Cancel),
    };

    let add_model_now = match prompt_select(
        terminal,
        "cx add",
        "是否立即为这个新 Provider 添加首个 model",
        &[
            "先保存 Provider".to_string(),
            "继续添加首个 model".to_string(),
        ],
        0,
        "↑/↓ 或 j/k 移动  ·  Enter 确认  ·  Esc 返回  ·  q 退出",
    )? {
        PromptOutcome::Submit(0) => false,
        PromptOutcome::Submit(1) => true,
        PromptOutcome::Back => return Ok(PromptOutcome::Back),
        PromptOutcome::Cancel => return Ok(PromptOutcome::Cancel),
        PromptOutcome::Submit(_) => unreachable!(),
    };

    let first_wire_api = [WireApi::Anthropic, WireApi::Responses, WireApi::Completions]
        .into_iter()
        .find(|wire_api| endpoints.contains_key(wire_api.display()))
        .context("至少需要一个 wire_api endpoint")?;
    let mut provider = ProviderConfig {
        name: provider_name.clone(),
        apikey_source,
        models: BTreeMap::new(),
        endpoints,
    };

    if add_model_now {
        match collect_model_draft(terminal, config, &provider, first_wire_api)? {
            PromptOutcome::Submit((model_id, model)) => {
                provider.models.insert(model_id, model);
            }
            PromptOutcome::Back => {}
            PromptOutcome::Cancel => return Ok(PromptOutcome::Cancel),
        }
    }

    let operation = AddOperation::Provider { provider };
    match confirm_add_operation(terminal, &operation, config_path)? {
        PromptOutcome::Submit(()) => Ok(PromptOutcome::Submit(operation)),
        PromptOutcome::Back => Ok(PromptOutcome::Back),
        PromptOutcome::Cancel => Ok(PromptOutcome::Cancel),
    }
}

fn collect_endpoint_operation(
    terminal: &mut AppTerminal,
    provider: &ProviderConfig,
    config_path: &Path,
) -> Result<PromptOutcome<AddOperation>> {
    let available_wire_apis = [WireApi::Anthropic, WireApi::Responses, WireApi::Completions]
        .into_iter()
        .filter(|wire_api| !provider.endpoints.contains_key(wire_api.display()))
        .collect::<Vec<_>>();

    if available_wire_apis.is_empty() {
        show_notice(
            terminal,
            "cx add",
            &format!("Provider `{}` 已配置所有可用 wire_api", provider.name),
        )?;
        return Ok(PromptOutcome::Back);
    }

    let wire_api = match prompt_wire_api_select(
        terminal,
        "cx add",
        &format!("为 `{}` 选择要新增的 wire_api", provider.name),
        &available_wire_apis,
    )? {
        PromptOutcome::Submit(wire_api) => wire_api,
        PromptOutcome::Back => return Ok(PromptOutcome::Back),
        PromptOutcome::Cancel => return Ok(PromptOutcome::Cancel),
    };

    let endpoint_url = match prompt_text(
        terminal,
        "cx add",
        &format!(
            "为 `{}` 输入 {} endpoint URL",
            provider.name,
            wire_api.display()
        ),
        "",
        "示例：https://dashscope.aliyuncs.com/compatible-mode/v1",
        validate_endpoint_url,
    )? {
        PromptOutcome::Submit(value) => value,
        PromptOutcome::Back => return Ok(PromptOutcome::Back),
        PromptOutcome::Cancel => return Ok(PromptOutcome::Cancel),
    };

    let operation = AddOperation::Endpoint {
        provider_name: provider.name.clone(),
        wire_api,
        endpoint: ProviderEndpointSpec::Url(endpoint_url),
    };
    match confirm_add_operation(terminal, &operation, config_path)? {
        PromptOutcome::Submit(()) => Ok(PromptOutcome::Submit(operation)),
        PromptOutcome::Back => Ok(PromptOutcome::Back),
        PromptOutcome::Cancel => Ok(PromptOutcome::Cancel),
    }
}

fn collect_model_operation(
    terminal: &mut AppTerminal,
    config: &CxConfig,
    provider: &ProviderConfig,
    config_path: &Path,
) -> Result<PromptOutcome<AddOperation>> {
    let endpoints = provider.normalized_endpoints();
    if endpoints.is_empty() {
        show_notice(
            terminal,
            "cx add",
            &format!(
                "Provider `{}` 还没有 endpoint，请先添加 wire_api",
                provider.name
            ),
        )?;
        return Ok(PromptOutcome::Back);
    }

    let endpoint_items = endpoints
        .iter()
        .map(|endpoint| format!("{:<11} {}", endpoint.wire_api, endpoint.url))
        .collect::<Vec<_>>();
    let wire_api = match prompt_select(
        terminal,
        "cx add",
        &format!("为 `{}` 选择要挂载 model 的 endpoint", provider.name),
        &endpoint_items,
        0,
        "↑/↓ 或 j/k 移动  ·  Enter 确认  ·  Esc 返回  ·  q 退出",
    )? {
        PromptOutcome::Submit(index) => WireApi::from_str(&endpoints[index].wire_api),
        PromptOutcome::Back => return Ok(PromptOutcome::Back),
        PromptOutcome::Cancel => return Ok(PromptOutcome::Cancel),
    };

    let (model_id, model) = match collect_model_draft(terminal, config, provider, wire_api)? {
        PromptOutcome::Submit(model) => model,
        PromptOutcome::Back => return Ok(PromptOutcome::Back),
        PromptOutcome::Cancel => return Ok(PromptOutcome::Cancel),
    };

    let operation = AddOperation::Model {
        provider_name: provider.name.clone(),
        wire_api,
        model_id,
        model,
    };
    match confirm_add_operation(terminal, &operation, config_path)? {
        PromptOutcome::Submit(()) => Ok(PromptOutcome::Submit(operation)),
        PromptOutcome::Back => Ok(PromptOutcome::Back),
        PromptOutcome::Cancel => Ok(PromptOutcome::Cancel),
    }
}

fn collect_model_draft(
    terminal: &mut AppTerminal,
    config: &CxConfig,
    provider: &ProviderConfig,
    wire_api: WireApi,
) -> Result<PromptOutcome<(String, ProviderModelConfig)>> {
    let model_id = match prompt_text(
        terminal,
        "cx add",
        &format!(
            "为 `{}` 的 `{}` endpoint 输入 model id",
            provider.name,
            wire_api.display()
        ),
        "",
        "示例：qwen3.6-plus / claude-opus-4-7 / mimo-v2.5-pro",
        |value| validate_model_id(provider, value),
    )? {
        PromptOutcome::Submit(value) => value,
        PromptOutcome::Back => return Ok(PromptOutcome::Back),
        PromptOutcome::Cancel => return Ok(PromptOutcome::Cancel),
    };

    let model_agents = match prompt_multi_select(
        terminal,
        "cx add",
        "选择 model 可见的 agent；留空表示继承 Provider/endpoint 过滤",
        &compatible_agents_for_wire_api(config, wire_api),
        &[],
        true,
    )? {
        PromptOutcome::Submit(selected) => selected,
        PromptOutcome::Back => return Ok(PromptOutcome::Back),
        PromptOutcome::Cancel => return Ok(PromptOutcome::Cancel),
    };

    let swe_pro = match prompt_text(
        terminal,
        "cx add",
        "可选：输入 SWE-bench Pro 成绩；留空则不写入",
        "",
        "示例：45.3%",
        |value| Ok(value.trim().to_string()),
    )? {
        PromptOutcome::Submit(value) => value,
        PromptOutcome::Back => return Ok(PromptOutcome::Back),
        PromptOutcome::Cancel => return Ok(PromptOutcome::Cancel),
    };

    let lcb_pro = match prompt_text(
        terminal,
        "cx add",
        "可选：输入 LiveCodeBench Pro 成绩；留空则不写入",
        "",
        "示例：2085",
        |value| Ok(value.trim().to_string()),
    )? {
        PromptOutcome::Submit(value) => value,
        PromptOutcome::Back => return Ok(PromptOutcome::Back),
        PromptOutcome::Cancel => return Ok(PromptOutcome::Cancel),
    };

    let hle = match prompt_text(
        terminal,
        "cx add",
        "可选：输入 Humanity's Last Exam 成绩；留空则不写入",
        "",
        "示例：30.2%",
        |value| Ok(value.trim().to_string()),
    )? {
        PromptOutcome::Submit(value) => value,
        PromptOutcome::Back => return Ok(PromptOutcome::Back),
        PromptOutcome::Cancel => return Ok(PromptOutcome::Cancel),
    };

    let desc = match prompt_text(
        terminal,
        "cx add",
        "可选：输入 model 描述；留空则不写入",
        "",
        "示例：Agent/终端最强",
        |value| Ok(value.trim().to_string()),
    )? {
        PromptOutcome::Submit(value) => value,
        PromptOutcome::Back => return Ok(PromptOutcome::Back),
        PromptOutcome::Cancel => return Ok(PromptOutcome::Cancel),
    };

    let context = match prompt_text(
        terminal,
        "cx add",
        "可选：输入 model 上下文大小；留空则不写入",
        "",
        "示例：1M, 128K, 200K",
        |value| Ok(value.trim().to_string()),
    )? {
        PromptOutcome::Submit(value) => value,
        PromptOutcome::Back => return Ok(PromptOutcome::Back),
        PromptOutcome::Cancel => return Ok(PromptOutcome::Cancel),
    };

    Ok(PromptOutcome::Submit((
        model_id,
        ProviderModelConfig {
            swe_pro: empty_string_as_none(&swe_pro),
            lcb_pro: empty_string_as_none(&lcb_pro),
            hle: empty_string_as_none(&hle),
            desc: empty_string_as_none(&desc),
            context: empty_string_as_none(&context),
            wire_apis: vec![wire_api.display().to_string()],
            agents: model_agents,
            env: BTreeMap::new(),
        },
    )))
}

fn empty_string_as_none(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn confirm_add_operation(
    terminal: &mut AppTerminal,
    operation: &AddOperation,
    config_path: &Path,
) -> Result<PromptOutcome<()>> {
    let preview = add_operation_preview(operation)?;
    prompt_summary(
        terminal,
        "cx add",
        &format!("确认写入 {}", config_path.display()),
        &preview,
    )
}

fn prompt_wire_api_select(
    terminal: &mut AppTerminal,
    title: &str,
    subtitle: &str,
    available: &[WireApi],
) -> Result<PromptOutcome<WireApi>> {
    let items = available
        .iter()
        .map(|wire_api| wire_api.display().to_string())
        .collect::<Vec<_>>();
    match prompt_select(
        terminal,
        title,
        subtitle,
        &items,
        0,
        "↑/↓ 或 j/k 移动  ·  Enter 确认  ·  Esc 返回  ·  q 退出",
    )? {
        PromptOutcome::Submit(index) => Ok(PromptOutcome::Submit(available[index])),
        PromptOutcome::Back => Ok(PromptOutcome::Back),
        PromptOutcome::Cancel => Ok(PromptOutcome::Cancel),
    }
}

fn prompt_provider_endpoint_form(
    terminal: &mut AppTerminal,
    title: &str,
    subtitle: &str,
) -> Result<PromptOutcome<BTreeMap<String, ProviderEndpointSpec>>> {
    let fields = [WireApi::Anthropic, WireApi::Responses, WireApi::Completions];
    let mut values = vec![String::new(), String::new(), String::new()];
    let mut index = 0usize;
    let mut error = None::<String>;

    loop {
        terminal
            .draw(|frame| {
                render_provider_endpoint_form(
                    frame,
                    title,
                    subtitle,
                    &fields,
                    &values,
                    index,
                    error.as_deref(),
                )
            })
            .context("绘制 Provider endpoint 表单失败")?;

        match event::read().context("读取终端事件失败")? {
            Event::Paste(text) => {
                append_paste_chunk(&mut values[index], &text);
                error = None;
            }
            Event::Key(key) => {
                if key.kind != KeyEventKind::Press {
                    continue;
                }

                match key.code {
                    KeyCode::Up => {
                        index = if index == 0 {
                            fields.len() - 1
                        } else {
                            index - 1
                        };
                    }
                    KeyCode::Down | KeyCode::Tab => {
                        index = (index + 1) % fields.len();
                    }
                    KeyCode::BackTab => {
                        index = if index == 0 {
                            fields.len() - 1
                        } else {
                            index - 1
                        };
                    }
                    KeyCode::Enter => {
                        let inputs = fields
                            .iter()
                            .copied()
                            .zip(values.iter().cloned())
                            .collect::<Vec<_>>();
                        match build_provider_endpoints_from_inputs(&inputs) {
                            Ok(endpoints) => return Ok(PromptOutcome::Submit(endpoints)),
                            Err(err) => {
                                error = Some(err.to_string());
                                continue;
                            }
                        }
                    }
                    KeyCode::Esc => return Ok(PromptOutcome::Back),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(PromptOutcome::Cancel);
                    }
                    KeyCode::Char('q') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(PromptOutcome::Cancel);
                    }
                    KeyCode::Char(ch) => {
                        values[index].push(ch);
                        error = None;
                    }
                    KeyCode::Backspace => {
                        values[index].pop();
                        error = None;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

fn append_paste_chunk(value: &mut String, pasted: &str) {
    value.extend(pasted.chars().filter(|ch| *ch != '\r' && *ch != '\n'));
}

fn build_provider_endpoints_from_inputs(
    values: &[(WireApi, String)],
) -> Result<BTreeMap<String, ProviderEndpointSpec>> {
    let mut endpoints = BTreeMap::new();
    for (wire_api, raw) in values {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let url = validate_endpoint_url(raw)?;
        endpoints.insert(
            wire_api.display().to_string(),
            ProviderEndpointSpec::Url(url),
        );
    }
    if endpoints.is_empty() {
        bail!("至少填写一个 wire_api endpoint URL");
    }
    Ok(endpoints)
}

fn handle_text_input_event(value: &mut String, event: &Event) -> TextInputAction {
    match event {
        Event::Paste(text) => {
            append_paste_chunk(value, text);
            TextInputAction::Changed
        }
        Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
            KeyCode::Enter => TextInputAction::Submit,
            KeyCode::Esc => TextInputAction::Back,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                TextInputAction::Cancel
            }
            KeyCode::Char('q') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                TextInputAction::Cancel
            }
            KeyCode::Char(ch) => {
                value.push(ch);
                TextInputAction::Changed
            }
            KeyCode::Backspace => {
                value.pop();
                TextInputAction::Changed
            }
            _ => TextInputAction::None,
        },
        _ => TextInputAction::None,
    }
}

fn prompt_select(
    terminal: &mut AppTerminal,
    title: &str,
    subtitle: &str,
    items: &[String],
    initial_index: usize,
    footer: &str,
) -> Result<PromptOutcome<usize>> {
    let mut index = if items.is_empty() {
        0
    } else {
        initial_index.min(items.len().saturating_sub(1))
    };

    loop {
        terminal
            .draw(|frame| render_select_prompt(frame, title, subtitle, items, index, footer))
            .context("绘制选择列表失败")?;

        if let Event::Key(key) = event::read().context("读取终端事件失败")? {
            if key.kind != KeyEventKind::Press {
                continue;
            }

            match key.code {
                KeyCode::Up | KeyCode::Char('k') if !items.is_empty() => {
                    index = if index == 0 {
                        items.len() - 1
                    } else {
                        index - 1
                    };
                }
                KeyCode::Down | KeyCode::Char('j') if !items.is_empty() => {
                    index = (index + 1) % items.len();
                }
                KeyCode::Enter if !items.is_empty() => return Ok(PromptOutcome::Submit(index)),
                KeyCode::Esc | KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => {
                    return Ok(PromptOutcome::Back);
                }
                KeyCode::Char('q') => return Ok(PromptOutcome::Cancel),
                _ => {}
            }
        }
    }
}

fn prompt_text<F>(
    terminal: &mut AppTerminal,
    title: &str,
    subtitle: &str,
    initial: &str,
    help: &str,
    validator: F,
) -> Result<PromptOutcome<String>>
where
    F: Fn(&str) -> Result<String>,
{
    let mut value = initial.to_string();
    let mut error = None::<String>;

    loop {
        terminal
            .draw(|frame| {
                render_text_prompt(frame, title, subtitle, &value, help, error.as_deref())
            })
            .context("绘制文本输入失败")?;

        let event = event::read().context("读取终端事件失败")?;
        match handle_text_input_event(&mut value, &event) {
            TextInputAction::Submit => match validator(&value) {
                Ok(validated) => return Ok(PromptOutcome::Submit(validated)),
                Err(err) => error = Some(err.to_string()),
            },
            TextInputAction::Back => return Ok(PromptOutcome::Back),
            TextInputAction::Cancel => return Ok(PromptOutcome::Cancel),
            TextInputAction::Changed => error = None,
            TextInputAction::None => {}
        }
    }
}

fn prompt_multi_select(
    terminal: &mut AppTerminal,
    title: &str,
    subtitle: &str,
    options: &[String],
    initial_selected: &[String],
    allow_empty: bool,
) -> Result<PromptOutcome<Vec<String>>> {
    let mut index = 0usize;
    let mut selected = options
        .iter()
        .map(|option| initial_selected.iter().any(|item| item == option))
        .collect::<Vec<_>>();
    let mut error = None::<String>;

    loop {
        terminal
            .draw(|frame| {
                render_multi_select_prompt(
                    frame,
                    &MultiSelectPrompt {
                        title,
                        subtitle,
                        options,
                        selected: &selected,
                        index,
                        allow_empty,
                        error: error.as_deref(),
                    },
                )
            })
            .context("绘制多选输入失败")?;

        if let Event::Key(key) = event::read().context("读取终端事件失败")? {
            if key.kind != KeyEventKind::Press {
                continue;
            }

            match key.code {
                KeyCode::Up | KeyCode::Char('k') if !options.is_empty() => {
                    index = if index == 0 {
                        options.len() - 1
                    } else {
                        index - 1
                    };
                }
                KeyCode::Down | KeyCode::Char('j') if !options.is_empty() => {
                    index = (index + 1) % options.len();
                }
                KeyCode::Char(' ') if !options.is_empty() => {
                    selected[index] = !selected[index];
                    error = None;
                }
                KeyCode::Enter => {
                    let values = options
                        .iter()
                        .enumerate()
                        .filter_map(|(idx, option)| selected[idx].then_some(option.clone()))
                        .collect::<Vec<_>>();
                    if !allow_empty && values.is_empty() {
                        error = Some("至少选择一项".to_string());
                    } else {
                        return Ok(PromptOutcome::Submit(values));
                    }
                }
                KeyCode::Esc | KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => {
                    return Ok(PromptOutcome::Back);
                }
                KeyCode::Char('q') => return Ok(PromptOutcome::Cancel),
                _ => {}
            }
        }
    }
}

fn prompt_summary(
    terminal: &mut AppTerminal,
    title: &str,
    subtitle: &str,
    preview: &str,
) -> Result<PromptOutcome<()>> {
    loop {
        terminal
            .draw(|frame| render_summary_prompt(frame, title, subtitle, preview))
            .context("绘制确认摘要失败")?;

        if let Event::Key(key) = event::read().context("读取终端事件失败")? {
            if key.kind != KeyEventKind::Press {
                continue;
            }

            match key.code {
                KeyCode::Enter => return Ok(PromptOutcome::Submit(())),
                KeyCode::Esc | KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => {
                    return Ok(PromptOutcome::Back);
                }
                KeyCode::Char('q') => return Ok(PromptOutcome::Cancel),
                _ => {}
            }
        }
    }
}

fn show_notice(terminal: &mut AppTerminal, title: &str, message: &str) -> Result<()> {
    loop {
        terminal
            .draw(|frame| render_summary_prompt(frame, title, message, "按 Enter / Esc 返回"))
            .context("绘制提示信息失败")?;

        if let Event::Key(key) = event::read().context("读取终端事件失败")? {
            if key.kind != KeyEventKind::Press {
                continue;
            }

            match key.code {
                KeyCode::Enter
                | KeyCode::Esc
                | KeyCode::Backspace
                | KeyCode::Left
                | KeyCode::Char('h')
                | KeyCode::Char('q') => return Ok(()),
                _ => {}
            }
        }
    }
}

fn render_prompt_frame(
    frame: &mut Frame<'_>,
    title: &str,
    subtitle: &str,
    footer: &str,
) -> [Rect; 4] {
    let area = frame.area();
    frame.render_widget(Clear, area);

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(2),
            Constraint::Min(8),
            Constraint::Length(3),
        ])
        .split(area);

    let title_widget = Paragraph::new("cx")
        .style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .block(Block::default().borders(Borders::ALL).title(title));
    frame.render_widget(title_widget, layout[0]);

    let subtitle_widget = Paragraph::new(subtitle)
        .style(Style::default().fg(Color::Yellow))
        .wrap(Wrap { trim: true });
    frame.render_widget(subtitle_widget, layout[1]);

    let footer_widget = Paragraph::new(footer)
        .style(Style::default().fg(Color::DarkGray))
        .wrap(Wrap { trim: true });
    frame.render_widget(footer_widget, layout[3]);

    [layout[0], layout[1], layout[2], layout[3]]
}

fn render_select_prompt(
    frame: &mut Frame<'_>,
    title: &str,
    subtitle: &str,
    items: &[String],
    index: usize,
    footer: &str,
) {
    let [_, _, body, _] = render_prompt_frame(frame, title, subtitle, footer);
    let list_items = items
        .iter()
        .map(|item| ListItem::new(item.clone()))
        .collect::<Vec<_>>();
    let mut list_state = ListState::default().with_selected((!items.is_empty()).then_some(index));
    let list = List::new(list_items)
        .block(Block::default().borders(Borders::ALL).title("选项"))
        .highlight_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("✨ ");
    frame.render_stateful_widget(list, body, &mut list_state);
}

fn render_text_prompt(
    frame: &mut Frame<'_>,
    title: &str,
    subtitle: &str,
    value: &str,
    help: &str,
    error: Option<&str>,
) {
    let [_, _, body, _] = render_prompt_frame(
        frame,
        title,
        subtitle,
        "输入文本  ·  Enter 确认  ·  Esc 返回  ·  q 退出",
    );
    let body_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(3)])
        .split(body);

    let input = Paragraph::new(value.to_string())
        .block(Block::default().borders(Borders::ALL).title("输入"))
        .wrap(Wrap { trim: false });
    frame.render_widget(input, body_layout[0]);
    frame.set_cursor_position((
        body_layout[0].x + 1 + value.chars().count() as u16,
        body_layout[0].y + 1,
    ));

    let mut note = help.to_string();
    if let Some(error) = error {
        note.push_str("\n\n");
        note.push_str(error);
    }
    let note_style = if error.is_some() {
        Style::default().fg(Color::Red)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let help_widget = Paragraph::new(note)
        .style(note_style)
        .block(Block::default().borders(Borders::ALL).title("说明"))
        .wrap(Wrap { trim: true });
    frame.render_widget(help_widget, body_layout[1]);
}

fn render_provider_endpoint_form(
    frame: &mut Frame<'_>,
    title: &str,
    subtitle: &str,
    wire_apis: &[WireApi],
    values: &[String],
    active_index: usize,
    error: Option<&str>,
) {
    let [_, _, body, _] = render_prompt_frame(
        frame,
        title,
        subtitle,
        "↑/↓ 或 Tab 切换字段  ·  输入或粘贴 URL  ·  Enter 确认  ·  Esc 返回  ·  Ctrl+C 退出",
    );
    let body_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(4),
        ])
        .split(body);

    for (index, wire_api) in wire_apis.iter().enumerate() {
        let is_active = index == active_index;
        let block = Block::default()
            .borders(Borders::ALL)
            .title(wire_api.display())
            .border_style(if is_active {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default()
            });
        let input = Paragraph::new(values[index].clone())
            .block(block)
            .wrap(Wrap { trim: false });
        frame.render_widget(input, body_layout[index]);

        if is_active {
            frame.set_cursor_position((
                body_layout[index].x + 1 + values[index].chars().count() as u16,
                body_layout[index].y + 1,
            ));
        }
    }

    let mut note = "留空表示不支持该 wire_api；至少填写一个有效的 endpoint URL。".to_string();
    if let Some(error) = error {
        note.push('\n');
        note.push_str(error);
    }
    let help_widget = Paragraph::new(note)
        .style(if error.is_some() {
            Style::default().fg(Color::Red)
        } else {
            Style::default().fg(Color::DarkGray)
        })
        .block(Block::default().borders(Borders::ALL).title("说明"))
        .wrap(Wrap { trim: true });
    frame.render_widget(help_widget, body_layout[3]);
}

struct MultiSelectPrompt<'a> {
    title: &'a str,
    subtitle: &'a str,
    options: &'a [String],
    selected: &'a [bool],
    index: usize,
    allow_empty: bool,
    error: Option<&'a str>,
}

fn render_multi_select_prompt(frame: &mut Frame<'_>, prompt: &MultiSelectPrompt<'_>) {
    let [_, _, body, _] = render_prompt_frame(
        frame,
        prompt.title,
        prompt.subtitle,
        "↑/↓ 或 j/k 移动  ·  Space 切换  ·  Enter 确认  ·  Esc 返回  ·  q 退出",
    );
    let body_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(6), Constraint::Length(4)])
        .split(body);

    let list_items = prompt
        .options
        .iter()
        .enumerate()
        .map(|(idx, option)| {
            let marker = if prompt.selected[idx] { "[x]" } else { "[ ]" };
            ListItem::new(format!("{marker} {option}"))
        })
        .collect::<Vec<_>>();
    let mut list_state =
        ListState::default().with_selected((!prompt.options.is_empty()).then_some(prompt.index));
    let list = List::new(list_items)
        .block(Block::default().borders(Borders::ALL).title("多选"))
        .highlight_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("✨ ");
    frame.render_stateful_widget(list, body_layout[0], &mut list_state);

    let mut help = if prompt.allow_empty {
        "留空表示不额外过滤。".to_string()
    } else {
        "至少选择一项。".to_string()
    };
    if let Some(error) = prompt.error {
        help.push('\n');
        help.push_str(error);
    }
    let help_widget = Paragraph::new(help)
        .style(if prompt.error.is_some() {
            Style::default().fg(Color::Red)
        } else {
            Style::default().fg(Color::DarkGray)
        })
        .block(Block::default().borders(Borders::ALL).title("说明"))
        .wrap(Wrap { trim: true });
    frame.render_widget(help_widget, body_layout[1]);
}

fn render_summary_prompt(frame: &mut Frame<'_>, title: &str, subtitle: &str, preview: &str) {
    let [_, _, body, _] = render_prompt_frame(
        frame,
        title,
        subtitle,
        "Enter 写入配置  ·  Esc 返回  ·  q 退出",
    );
    let summary = Paragraph::new(preview.to_string())
        .block(Block::default().borders(Borders::ALL).title("预览"))
        .wrap(Wrap { trim: false });
    frame.render_widget(summary, body);
}

// ══════════════════════════════════════════════════
// TUI
// ══════════════════════════════════════════════════

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Agent,
    Provider,
    Model,
}

struct AppState {
    step: Step,
    agent_hint: Option<String>,
    agent_index: usize,
    provider_index: usize,
    model_index: usize,
    model_wire_api_indexes: BTreeMap<String, usize>,
    selected_agent_id: String,
    config: CxConfig,
}

impl AppState {
    fn new(agent_hint: Option<String>, config: &CxConfig) -> Self {
        let first_agent = resolved_agents(config)
            .first()
            .map(|a| a.id.clone())
            .unwrap_or("copilot".to_string());
        let selected_agent_id = agent_hint.as_ref().cloned().unwrap_or(first_agent);
        let step = if agent_hint.is_some() {
            Step::Provider
        } else {
            Step::Agent
        };

        Self {
            step,
            agent_hint,
            agent_index: 0,
            provider_index: 0,
            model_index: 0,
            model_wire_api_indexes: BTreeMap::new(),
            selected_agent_id,
            config: config.clone(),
        }
    }

    fn resolved_agents(&self) -> Vec<ResolvedAgent> {
        resolved_agents(&self.config)
    }

    fn current_index(&self) -> usize {
        match self.step {
            Step::Agent => self.agent_index,
            Step::Provider => self.provider_index,
            Step::Model => self.model_index,
        }
    }

    fn current_items(&self, models: &[ResolvedModel]) -> Vec<String> {
        match self.step {
            Step::Agent => self
                .resolved_agents()
                .iter()
                .map(|a| {
                    let mut title = a.id.clone();
                    if let Some(first) = title.get_mut(0..1) {
                        first.make_ascii_uppercase();
                    }
                    title
                })
                .collect(),
            Step::Provider => providers_for_agent(&self.config, &self.selected_agent_id)
                .iter()
                .map(|p| p.name.clone())
                .collect(),
            Step::Model => self
                .current_model_options(models)
                .iter()
                .map(|model| model.formatted_row(&self.model_wire_api_indexes))
                .collect(),
        }
    }

    fn current_model_options(&self, models: &[ResolvedModel]) -> Vec<ModelOption> {
        let providers = providers_for_agent(&self.config, &self.selected_agent_id);
        let provider = &providers[self.provider_index];
        model_options_for_provider(models, &self.selected_agent_id, &provider.name)
    }

    fn move_up(&mut self, models: &[ResolvedModel]) {
        let len = self.current_items(models).len();
        if len == 0 {
            return;
        }
        match self.step {
            Step::Agent => {
                self.agent_index = if self.agent_index == 0 {
                    len - 1
                } else {
                    self.agent_index - 1
                }
            }
            Step::Provider => {
                self.provider_index = if self.provider_index == 0 {
                    len - 1
                } else {
                    self.provider_index - 1
                }
            }
            Step::Model => {
                self.model_index = if self.model_index == 0 {
                    len - 1
                } else {
                    self.model_index - 1
                }
            }
        }
    }

    fn move_down(&mut self, models: &[ResolvedModel]) {
        let len = self.current_items(models).len();
        if len == 0 {
            return;
        }
        match self.step {
            Step::Agent => self.agent_index = (self.agent_index + 1) % len,
            Step::Provider => self.provider_index = (self.provider_index + 1) % len,
            Step::Model => self.model_index = (self.model_index + 1) % len,
        }
    }

    fn cycle_model_wire_api(&mut self, models: &[ResolvedModel], move_right: bool) {
        if self.step != Step::Model {
            return;
        }

        let options = self.current_model_options(models);
        let Some(option) = options.get(self.model_index) else {
            return;
        };
        if option.variants.len() <= 1 {
            return;
        }

        let current = option.selected_variant_index(&self.model_wire_api_indexes);
        let next = if move_right {
            (current + 1) % option.variants.len()
        } else if current == 0 {
            option.variants.len() - 1
        } else {
            current - 1
        };
        self.model_wire_api_indexes
            .insert(option.selection_key.clone(), next);
    }

    fn confirm(&mut self, models: &[ResolvedModel]) -> Option<Selection> {
        match self.step {
            Step::Agent => {
                let agents = self.resolved_agents();
                if self.agent_index >= agents.len() {
                    return None;
                }
                let agent = &agents[self.agent_index];
                self.selected_agent_id = agent.id.clone();
                self.provider_index = 0;
                self.model_index = 0;
                self.step = Step::Provider;
                None
            }
            Step::Provider => {
                let providers = providers_for_agent(&self.config, &self.selected_agent_id);
                let provider = providers[self.provider_index].clone();
                if provider.requires_model() {
                    self.model_index = 0;
                    self.step = Step::Model;
                    None
                } else {
                    let agent = find_agent(&self.config, &self.selected_agent_id).unwrap();
                    Some(Selection {
                        agent_id: agent.id.clone(),
                        agent_binary: agent.binary.clone(),
                        agent_args: agent.args.clone(),
                        agent_env: agent.env.clone(),
                        provider,
                        model: None,
                    })
                }
            }
            Step::Model => {
                let providers = providers_for_agent(&self.config, &self.selected_agent_id);
                let provider = providers[self.provider_index].clone();
                let available = self.current_model_options(models);
                let option = available.get(self.model_index)?;
                let selected_variant = option
                    .selected_variant(&self.model_wire_api_indexes)
                    .clone();
                let agent = find_agent(&self.config, &self.selected_agent_id).unwrap();
                Some(Selection {
                    agent_id: agent.id.clone(),
                    agent_binary: agent.binary.clone(),
                    agent_args: agent.args.clone(),
                    agent_env: agent.env.clone(),
                    provider,
                    model: Some(selected_variant),
                })
            }
        }
    }

    fn go_back(&mut self) -> bool {
        match self.step {
            Step::Agent => true,
            Step::Provider => {
                if self.agent_hint.is_some() {
                    true
                } else {
                    self.step = Step::Agent;
                    false
                }
            }
            Step::Model => {
                self.step = Step::Provider;
                false
            }
        }
    }
}

fn run_tui(
    agent_hint: Option<String>,
    config: &CxConfig,
    models: &[ResolvedModel],
) -> Result<Option<Selection>> {
    enable_raw_mode().context("启用终端 raw mode 失败")?;
    let _terminal_guard = TerminalGuard;

    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste).context("进入备用屏幕失败")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("初始化终端失败")?;

    let mut state = AppState::new(agent_hint, config);

    loop {
        terminal
            .draw(|frame| render(frame, &state, models))
            .context("绘制 TUI 失败")?;

        if let Event::Key(key) = event::read().context("读取终端事件失败")? {
            if key.kind != KeyEventKind::Press {
                continue;
            }

            match key.code {
                KeyCode::Up | KeyCode::Char('k') => state.move_up(models),
                KeyCode::Down | KeyCode::Char('j') => state.move_down(models),
                KeyCode::Left if state.step == Step::Model => {
                    state.cycle_model_wire_api(models, false)
                }
                KeyCode::Right if state.step == Step::Model => {
                    state.cycle_model_wire_api(models, true)
                }
                KeyCode::Enter => {
                    if let Some(selection) = state.confirm(models) {
                        return Ok(Some(selection));
                    }
                }
                KeyCode::Esc | KeyCode::Backspace if state.go_back() => return Ok(None),
                KeyCode::Left | KeyCode::Char('h')
                    if state.step != Step::Model && state.go_back() =>
                {
                    return Ok(None);
                }
                KeyCode::Esc | KeyCode::Backspace => {}
                KeyCode::Left | KeyCode::Char('h') if state.step != Step::Model => {}
                KeyCode::Char('q') => return Ok(None),
                _ => {}
            }
        }
    }
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = execute!(stdout, LeaveAlternateScreen, DisableBracketedPaste);
    }
}

fn render(frame: &mut Frame<'_>, state: &AppState, models: &[ResolvedModel]) {
    let area = frame.area();
    frame.render_widget(Clear, area);

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(2),
            Constraint::Min(8),
            Constraint::Length(3),
        ])
        .split(area);

    let title = Paragraph::new("cx")
        .style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("统一 Agent 入口"),
        );
    frame.render_widget(title, layout[0]);

    let subtitle = Paragraph::new(current_prompt(state))
        .style(Style::default().fg(Color::Yellow))
        .wrap(Wrap { trim: true });
    frame.render_widget(subtitle, layout[1]);

    let items = state.current_items(models);
    let list_items = items
        .iter()
        .map(|item| ListItem::new(item.clone()))
        .collect::<Vec<_>>();
    let mut list_state = ListState::default().with_selected(Some(state.current_index()));
    let list = List::new(list_items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(current_title(state)),
        )
        .highlight_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("✨ ");
    frame.render_stateful_widget(list, layout[2], &mut list_state);

    let footer = Paragraph::new(current_footer(state))
        .style(Style::default().fg(Color::DarkGray))
        .wrap(Wrap { trim: true });
    frame.render_widget(footer, layout[3]);
}

fn current_title(state: &AppState) -> &'static str {
    match state.step {
        Step::Agent => "选择 Agent",
        Step::Provider => "选择 Provider",
        Step::Model => "选择 Model / wire_api",
    }
}

fn current_prompt(state: &AppState) -> String {
    match state.step {
        Step::Agent => "选择 Agent".to_string(),
        Step::Provider => "选择 Provider".to_string(),
        Step::Model => "选择 Model；上下切换模型，左右切换 wire_api".to_string(),
    }
}

fn current_footer(state: &AppState) -> &'static str {
    match state.step {
        Step::Agent | Step::Provider => {
            "↑/↓ 或 j/k 移动  ·  Enter 确认  ·  Esc/Backspace/← 返回  ·  q 退出"
        }
        Step::Model => {
            "↑/↓ 或 j/k 选择模型  ·  ←/→ 切换 wire_api  ·  Enter 确认  ·  Esc/Backspace 返回  ·  q 退出"
        }
    }
}

// ══════════════════════════════════════════════════
// Tests
// ══════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_test_config() -> CxConfig {
        CxConfig {
            providers: vec![ProviderConfig {
                name: "Test".into(),
                apikey_source: Some("literal:test".into()),
                models: BTreeMap::new(),
                endpoints: BTreeMap::new(),
            }],
            agents: vec![
                AgentConfig {
                    id: "copilot".into(),
                    binary: "copilot".into(),
                    args: vec![],
                    wire_apis: vec![],
                    env: BTreeMap::new(),
                },
                AgentConfig {
                    id: "claude".into(),
                    binary: "claude".into(),
                    args: vec![],
                    wire_apis: vec![],
                    env: BTreeMap::new(),
                },
                AgentConfig {
                    id: "codex".into(),
                    binary: "codex".into(),
                    args: vec![],
                    wire_apis: vec![],
                    env: BTreeMap::new(),
                },
            ],
        }
    }

    fn temp_test_dir(label: &str) -> PathBuf {
        env::temp_dir().join(format!("cx-{label}-{}", random_urlsafe(6)))
    }

    fn create_fake_binary(name: &str) -> PathBuf {
        let dir = temp_test_dir("fake-binary");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn test_resolved_model(model_id: &str, endpoint_url: &str, wire_api: WireApi) -> ResolvedModel {
        ResolvedModel {
            id: model_id.into(),
            swe_pro: "—".into(),
            lcb_pro: "—".into(),
            hle: "—".into(),
            desc: String::new(),
            context: "—".into(),
            wire_api,
            provider_name: "DashScope".into(),
            endpoint_url: endpoint_url.into(),
            visible_agents: vec!["codex".into(), "claude".into()],
            copilot_auth: CopilotAuth::ApiKey,
            env: BTreeMap::new(),
        }
    }

    fn multi_wire_api_test_config() -> CxConfig {
        CxConfig {
            providers: vec![ProviderConfig {
                name: "Xiaomi MIMO".into(),
                apikey_source: Some("literal:test".into()),
                models: BTreeMap::from([(
                    "mimo-v2.5-pro".into(),
                    ProviderModelConfig {
                        swe_pro: Some("80%".into()),
                        lcb_pro: Some("2100".into()),
                        hle: Some("70%".into()),
                        desc: Some("thinking".into()),
                        context: None,
                        wire_apis: vec![],
                        agents: Vec::new(),
                        env: BTreeMap::new(),
                    },
                )]),
                endpoints: BTreeMap::from([
                    (
                        "anthropic".into(),
                        ProviderEndpointSpec::Url("https://example.com/anthropic".into()),
                    ),
                    (
                        "completions".into(),
                        ProviderEndpointSpec::Url("https://example.com/v1".into()),
                    ),
                ]),
            }],
            agents: default_agent_configs(),
        }
    }

    // ── clap CLI parsing tests ──

    fn parse(args: &[&str]) -> Option<CxCommand> {
        Cli::try_parse_from(std::iter::once("cx").chain(args.iter().copied()))
            .ok()
            .and_then(|cli| cli.command)
    }

    fn dispatch(args: &[&str]) -> DispatchCommand {
        let raw_args = std::iter::once("cx".to_string())
            .chain(args.iter().map(|arg| (*arg).to_string()))
            .collect::<Vec<_>>();
        dispatch_command(&raw_args)
    }

    #[test]
    fn clap_parse_help() {
        assert_eq!(parse(&["help"]), Some(CxCommand::Help));
    }

    #[test]
    fn zero_args_dispatch_to_launcher() {
        assert_eq!(dispatch(&[]), DispatchCommand::Launch { args: Vec::new() });
    }

    #[test]
    fn clap_parse_probe_no_model() {
        assert_eq!(
            parse(&["probe"]),
            Some(CxCommand::Probe {
                provider: None,
                auto_probe: false,
            })
        );
    }

    #[test]
    fn clap_parse_probe_with_provider() {
        assert_eq!(
            parse(&["probe", "--provider", "百炼"]),
            Some(CxCommand::Probe {
                provider: Some("百炼".into()),
                auto_probe: false,
            })
        );
    }

    #[test]
    fn clap_parse_patch_url() {
        assert_eq!(
            parse(&["patch", "--url", "https://example.com/p.yaml"]),
            Some(CxCommand::Patch {
                source: None,
                url: Some("https://example.com/p.yaml".into()),
                refresh: false
            })
        );
    }

    #[test]
    fn clap_parse_patch_source_path() {
        assert_eq!(
            parse(&["patch", "./config/providers.default.yaml"]),
            Some(CxCommand::Patch {
                source: Some("./config/providers.default.yaml".into()),
                url: None,
                refresh: false
            })
        );
    }

    #[test]
    fn clap_parse_patch_refresh() {
        assert_eq!(
            parse(&["patch", "--refresh"]),
            Some(CxCommand::Patch {
                source: None,
                url: None,
                refresh: true
            })
        );
    }

    #[test]
    fn clap_parse_add() {
        assert_eq!(parse(&["add"]), Some(CxCommand::Add));
    }

    #[test]
    fn clap_unknown_subcommand_falls_through() {
        assert!(parse(&["claude", "mcp", "list"]).is_none());
        assert!(parse(&["unknown-cmd"]).is_none());
    }

    #[test]
    fn dispatch_help_stays_help() {
        assert_eq!(dispatch(&["help"]), DispatchCommand::Help);
    }

    #[test]
    fn dispatch_unknown_subcommand_falls_through_to_launcher() {
        assert_eq!(
            dispatch(&["claude", "mcp", "list"]),
            DispatchCommand::Launch {
                args: vec!["claude".into(), "mcp".into(), "list".into()]
            }
        );
    }

    // ── Launch spec tests ──

    #[test]
    fn codex_mcp_passthrough_stays_raw_without_endpoints() {
        let fake_binary = create_fake_binary("codex");
        let selection = Selection {
            agent_id: "codex".into(),
            agent_binary: fake_binary.display().to_string(),
            agent_args: Vec::new(),
            agent_env: BTreeMap::new(),
            provider: ResolvedProvider {
                name: "Codex Default".into(),
                has_endpoints: false,
                apikey_source: None,
            },
            model: None,
        };

        let spec = build_launch_spec(&selection, &["mcp".into(), "serve".into()]).unwrap();
        assert_eq!(spec.args, vec!["mcp".to_string(), "serve".to_string()]);
        let _ = fs::remove_dir_all(fake_binary.parent().unwrap());
    }

    #[test]
    fn claude_launch_removes_anthropic_env_vars() {
        let fake_binary = create_fake_binary("claude");
        let selection = Selection {
            agent_id: "claude".into(),
            agent_binary: fake_binary.display().to_string(),
            agent_args: Vec::new(),
            agent_env: BTreeMap::new(),
            provider: ResolvedProvider {
                name: "Test".into(),
                has_endpoints: false,
                apikey_source: Some("literal:test-key".into()),
            },
            model: None,
        };
        let spec = build_launch_spec(&selection, &[]).unwrap();
        assert!(spec.env_remove.contains(&"ANTHROPIC_API_KEY".to_string()));
        assert!(
            spec.env_remove
                .contains(&"ANTHROPIC_AUTH_TOKEN".to_string())
        );
        assert!(spec.env_remove.contains(&"ANTHROPIC_BASE_URL".to_string()));
        assert!(spec.env_remove.contains(&"ANTHROPIC_MODEL".to_string()));
        assert_eq!(spec.env.get("ANTHROPIC_API_KEY"), Some(&"test-key".into()));
        assert_eq!(
            spec.env.get("ANTHROPIC_AUTH_TOKEN"),
            Some(&"test-key".into())
        );
        assert!(!spec.env.contains_key("HOME"));
        let _ = fs::remove_dir_all(fake_binary.parent().unwrap());
    }

    #[test]
    fn claude_with_endpoint_removes_anthropic_env_vars() {
        let fake_binary = create_fake_binary("claude");
        let selection = Selection {
            agent_id: "claude".into(),
            agent_binary: fake_binary.display().to_string(),
            agent_args: Vec::new(),
            agent_env: BTreeMap::new(),
            provider: ResolvedProvider {
                name: "DashScope".into(),
                has_endpoints: true,
                apikey_source: Some("literal:test-key".into()),
            },
            model: Some(ResolvedModel {
                id: "qwen3.6-plus".into(),
                swe_pro: "—".into(),
                lcb_pro: "—".into(),
                hle: "—".into(),
                desc: String::new(),
                context: "—".into(),
                wire_api: WireApi::Anthropic,
                provider_name: "DashScope".into(),
                endpoint_url: "https://dashscope.aliyuncs.com/apps/anthropic".into(),
                visible_agents: vec!["claude".into()],
                copilot_auth: CopilotAuth::ApiKey,
                env: BTreeMap::new(),
            }),
        };
        let spec = build_launch_spec(&selection, &["mcp".into(), "list".into()]).unwrap();
        assert!(spec.env_remove.contains(&"ANTHROPIC_API_KEY".to_string()));
        assert!(
            spec.env_remove
                .contains(&"ANTHROPIC_AUTH_TOKEN".to_string())
        );
        assert!(spec.env_remove.contains(&"ANTHROPIC_BASE_URL".to_string()));
        assert!(spec.env_remove.contains(&"ANTHROPIC_MODEL".to_string()));
        assert_eq!(
            spec.env.get("ANTHROPIC_BASE_URL"),
            Some(&"https://dashscope.aliyuncs.com/apps/anthropic".into())
        );
        assert_eq!(spec.env.get("ANTHROPIC_API_KEY"), Some(&"test-key".into()));
        assert_eq!(
            spec.env.get("ANTHROPIC_MODEL"),
            Some(&"qwen3.6-plus".into())
        );
        assert_eq!(
            spec.args,
            vec![
                "--model".to_string(),
                "qwen3.6-plus".to_string(),
                "mcp".to_string(),
                "list".to_string()
            ]
        );
        let _ = fs::remove_dir_all(fake_binary.parent().unwrap());
    }

    #[test]
    fn copilot_with_bearer_auth_sets_bearer_token_env() {
        let fake_binary = create_fake_binary("copilot");
        let selection = Selection {
            agent_id: "copilot".into(),
            agent_binary: fake_binary.display().to_string(),
            agent_args: Vec::new(),
            agent_env: BTreeMap::new(),
            provider: ResolvedProvider {
                name: "Packy API".into(),
                has_endpoints: true,
                apikey_source: Some("literal:test-key".into()),
            },
            model: Some(ResolvedModel {
                id: "claude-opus-4-7".into(),
                swe_pro: "—".into(),
                lcb_pro: "—".into(),
                hle: "—".into(),
                desc: String::new(),
                context: "—".into(),
                wire_api: WireApi::Anthropic,
                provider_name: "Packy API".into(),
                endpoint_url: "https://www.packyapi.com/".into(),
                visible_agents: vec!["copilot".into()],
                copilot_auth: CopilotAuth::BearerToken,
                env: BTreeMap::new(),
            }),
        };

        let spec = build_launch_spec(&selection, &[]).unwrap();
        assert_eq!(
            spec.env.get("COPILOT_PROVIDER_BEARER_TOKEN"),
            Some(&"test-key".to_string())
        );
        assert!(!spec.env.contains_key("COPILOT_PROVIDER_API_KEY"));
        assert_eq!(
            spec.env.get("COPILOT_PROVIDER_TYPE"),
            Some(&"anthropic".to_string())
        );
        assert_eq!(
            spec.env.get("COPILOT_PROVIDER_BASE_URL"),
            Some(&"https://www.packyapi.com/".to_string())
        );
        let _ = fs::remove_dir_all(fake_binary.parent().unwrap());
    }

    #[test]
    fn sanitize_terminal_title_strips_control_chars() {
        assert_eq!(
            sanitize_terminal_title("gpt-5.4\x1b]2;ignored\x07\n"),
            "gpt-5.4]2;ignored"
        );
    }

    #[test]
    fn apply_selected_model_tab_name_skips_missing_model() {
        let selection = Selection {
            agent_id: "codex".into(),
            agent_binary: "codex".into(),
            agent_args: Vec::new(),
            agent_env: BTreeMap::new(),
            provider: ResolvedProvider {
                name: "Default".into(),
                has_endpoints: false,
                apikey_source: None,
            },
            model: None,
        };

        assert!(apply_selected_model_tab_name(&selection).is_ok());
    }

    // ── env injection tests ──

    #[test]
    fn agent_env_is_injected_into_launch_spec() {
        let fake_binary = create_fake_binary("claude");
        let mut agent_env = BTreeMap::new();
        agent_env.insert("MY_AGENT_VAR".into(), "from-agent".into());

        let selection = Selection {
            agent_id: "claude".into(),
            agent_binary: fake_binary.display().to_string(),
            agent_args: Vec::new(),
            agent_env,
            provider: ResolvedProvider {
                name: "Test".into(),
                has_endpoints: false,
                apikey_source: Some("literal:test-key".into()),
            },
            model: None,
        };
        let spec = build_launch_spec(&selection, &[]).unwrap();
        assert_eq!(spec.env.get("MY_AGENT_VAR"), Some(&"from-agent".into()));
        let _ = fs::remove_dir_all(fake_binary.parent().unwrap());
    }

    #[test]
    fn model_env_is_injected_into_launch_spec() {
        let fake_binary = create_fake_binary("claude");
        let mut model_env = BTreeMap::new();
        model_env.insert("CLAUDE_CODE_AUTO_COMPACT_WINDOW".into(), "1000000".into());

        let selection = Selection {
            agent_id: "claude".into(),
            agent_binary: fake_binary.display().to_string(),
            agent_args: Vec::new(),
            agent_env: BTreeMap::new(),
            provider: ResolvedProvider {
                name: "DashScope".into(),
                has_endpoints: true,
                apikey_source: Some("literal:test-key".into()),
            },
            model: Some(ResolvedModel {
                id: "glm-5.1".into(),
                swe_pro: "—".into(),
                lcb_pro: "—".into(),
                hle: "—".into(),
                desc: String::new(),
                context: "—".into(),
                wire_api: WireApi::Anthropic,
                provider_name: "DashScope".into(),
                endpoint_url: "https://dashscope.aliyuncs.com/apps/anthropic".into(),
                visible_agents: vec!["claude".into()],
                copilot_auth: CopilotAuth::ApiKey,
                env: model_env,
            }),
        };
        let spec = build_launch_spec(&selection, &[]).unwrap();
        assert_eq!(
            spec.env.get("CLAUDE_CODE_AUTO_COMPACT_WINDOW"),
            Some(&"1000000".into())
        );
        let _ = fs::remove_dir_all(fake_binary.parent().unwrap());
    }

    #[test]
    fn model_env_overrides_agent_env_on_collision() {
        let fake_binary = create_fake_binary("claude");
        let mut agent_env = BTreeMap::new();
        agent_env.insert("SHARED_VAR".into(), "from-agent".into());
        agent_env.insert("AGENT_ONLY_VAR".into(), "agent-value".into());

        let mut model_env = BTreeMap::new();
        model_env.insert("SHARED_VAR".into(), "from-model".into());
        model_env.insert("MODEL_ONLY_VAR".into(), "model-value".into());

        let selection = Selection {
            agent_id: "claude".into(),
            agent_binary: fake_binary.display().to_string(),
            agent_args: Vec::new(),
            agent_env,
            provider: ResolvedProvider {
                name: "DashScope".into(),
                has_endpoints: true,
                apikey_source: Some("literal:test-key".into()),
            },
            model: Some(ResolvedModel {
                id: "glm-5.1".into(),
                swe_pro: "—".into(),
                lcb_pro: "—".into(),
                hle: "—".into(),
                desc: String::new(),
                context: "—".into(),
                wire_api: WireApi::Anthropic,
                provider_name: "DashScope".into(),
                endpoint_url: "https://dashscope.aliyuncs.com/apps/anthropic".into(),
                visible_agents: vec!["claude".into()],
                copilot_auth: CopilotAuth::ApiKey,
                env: model_env,
            }),
        };
        let spec = build_launch_spec(&selection, &[]).unwrap();
        // Model env overrides agent env for the shared key
        assert_eq!(spec.env.get("SHARED_VAR"), Some(&"from-model".into()));
        // Both unique keys are present
        assert_eq!(spec.env.get("AGENT_ONLY_VAR"), Some(&"agent-value".into()));
        assert_eq!(spec.env.get("MODEL_ONLY_VAR"), Some(&"model-value".into()));
        let _ = fs::remove_dir_all(fake_binary.parent().unwrap());
    }

    // ── Merge tests ──

    #[test]
    fn merge_providers_replaces_by_name() {
        let existing = vec![ProviderConfig {
            name: "A".into(),
            apikey_source: Some("literal:old".into()),
            models: BTreeMap::new(),
            endpoints: BTreeMap::new(),
        }];
        let incoming = vec![ProviderConfig {
            name: "A".into(),
            apikey_source: Some("literal:new".into()),
            models: BTreeMap::new(),
            endpoints: BTreeMap::new(),
        }];
        let merged = merge_providers(&existing, &incoming);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].apikey_source.as_deref(), Some("literal:new"));
    }

    #[test]
    fn merge_providers_appends_new() {
        let existing = vec![ProviderConfig {
            name: "A".into(),
            apikey_source: None,
            models: BTreeMap::new(),
            endpoints: BTreeMap::new(),
        }];
        let incoming = vec![ProviderConfig {
            name: "B".into(),
            apikey_source: None,
            models: BTreeMap::new(),
            endpoints: BTreeMap::new(),
        }];
        let merged = merge_providers(&existing, &incoming);
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn merge_providers_preserves_order_for_replacements() {
        let existing = vec![
            ProviderConfig {
                name: "A".into(),
                apikey_source: Some("literal:a".into()),
                models: BTreeMap::new(),
                endpoints: BTreeMap::new(),
            },
            ProviderConfig {
                name: "B".into(),
                apikey_source: Some("literal:old".into()),
                models: BTreeMap::new(),
                endpoints: BTreeMap::new(),
            },
            ProviderConfig {
                name: "C".into(),
                apikey_source: Some("literal:c".into()),
                models: BTreeMap::new(),
                endpoints: BTreeMap::new(),
            },
        ];
        let incoming = vec![
            ProviderConfig {
                name: "B".into(),
                apikey_source: Some("literal:new".into()),
                models: BTreeMap::new(),
                endpoints: BTreeMap::new(),
            },
            ProviderConfig {
                name: "D".into(),
                apikey_source: Some("literal:d".into()),
                models: BTreeMap::new(),
                endpoints: BTreeMap::new(),
            },
        ];

        let merged = merge_providers(&existing, &incoming);
        let names: Vec<&str> = merged
            .iter()
            .map(|provider| provider.name.as_str())
            .collect();
        assert_eq!(names, vec!["A", "B", "C", "D"]);
        assert_eq!(merged[1].apikey_source.as_deref(), Some("literal:new"));
    }

    #[test]
    fn merge_agents_replaces_by_id() {
        let existing = vec![AgentConfig {
            id: "claude".into(),
            binary: "claude-old".into(),
            args: vec![],
            wire_apis: vec![],
            env: BTreeMap::new(),
        }];
        let incoming = vec![AgentConfig {
            id: "claude".into(),
            binary: "claude-new".into(),
            args: vec![],
            wire_apis: vec![],
            env: BTreeMap::new(),
        }];
        let merged = merge_agents(&existing, &incoming);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].binary, "claude-new");
    }

    #[test]
    fn merge_agents_appends_new() {
        let existing = vec![AgentConfig {
            id: "copilot".into(),
            binary: "copilot".into(),
            args: vec![],
            wire_apis: vec![],
            env: BTreeMap::new(),
        }];
        let incoming = vec![AgentConfig {
            id: "codex".into(),
            binary: "codex".into(),
            args: vec![],
            wire_apis: vec![],
            env: BTreeMap::new(),
        }];
        let merged = merge_agents(&existing, &incoming);
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn merge_agents_preserves_order_for_replacements() {
        let existing = vec![
            AgentConfig {
                id: "copilot".into(),
                binary: "copilot".into(),
                args: vec![],
                wire_apis: vec![],
                env: BTreeMap::new(),
            },
            AgentConfig {
                id: "claude".into(),
                binary: "claude-old".into(),
                args: vec![],
                wire_apis: vec![],
                env: BTreeMap::new(),
            },
            AgentConfig {
                id: "codex".into(),
                binary: "codex".into(),
                args: vec![],
                wire_apis: vec![],
                env: BTreeMap::new(),
            },
        ];
        let incoming = vec![
            AgentConfig {
                id: "claude".into(),
                binary: "claude-new".into(),
                args: vec![],
                wire_apis: vec![],
                env: BTreeMap::new(),
            },
            AgentConfig {
                id: "gemini".into(),
                binary: "gemini".into(),
                args: vec![],
                wire_apis: vec![],
                env: BTreeMap::new(),
            },
        ];

        let merged = merge_agents(&existing, &incoming);
        let ids: Vec<&str> = merged.iter().map(|agent| agent.id.as_str()).collect();
        assert_eq!(ids, vec!["copilot", "claude", "codex", "gemini"]);
        assert_eq!(merged[1].binary, "claude-new");
    }

    #[test]
    fn apply_add_provider_appends_provider() {
        let mut config = minimal_test_config();
        let provider = ProviderConfig {
            name: "Packy API".into(),
            apikey_source: Some("env:PACKY_API_KEY".into()),
            models: BTreeMap::new(),
            endpoints: BTreeMap::from([(
                "anthropic".into(),
                ProviderEndpointSpec::Url("https://example.com/anthropic".into()),
            )]),
        };

        let result = apply_add_operation(&mut config, AddOperation::Provider { provider }).unwrap();
        assert!(matches!(result, AddResult::Provider { .. }));
        assert!(
            config
                .providers
                .iter()
                .any(|candidate| candidate.name == "Packy API")
        );
    }

    #[test]
    fn apply_add_endpoint_rejects_duplicate_wire_api() {
        let mut config = minimal_test_config();
        config.providers[0].endpoints.insert(
            "responses".into(),
            ProviderEndpointSpec::Url("https://example.com/v1".into()),
        );

        let error = apply_add_operation(
            &mut config,
            AddOperation::Endpoint {
                provider_name: "Test".into(),
                wire_api: WireApi::Responses,
                endpoint: ProviderEndpointSpec::Url("https://another.example.com/v1".into()),
            },
        )
        .unwrap_err();

        assert!(error.to_string().contains("已存在 `responses` endpoint"));
    }

    #[test]
    fn apply_add_model_inserts_selected_wire_api() {
        let mut config = minimal_test_config();
        config.providers[0].endpoints.insert(
            "responses".into(),
            ProviderEndpointSpec::Url("https://example.com/v1".into()),
        );

        let result = apply_add_operation(
            &mut config,
            AddOperation::Model {
                provider_name: "Test".into(),
                wire_api: WireApi::Responses,
                model_id: "qwen3.6-plus".into(),
                model: ProviderModelConfig {
                    swe_pro: Some("45.3%".into()),
                    lcb_pro: None,
                    hle: None,
                    desc: Some("Agent/终端最强".into()),
                    context: None,
                    wire_apis: vec!["responses".into()],
                    agents: vec!["codex".into()],
                    env: BTreeMap::new(),
                },
            },
        )
        .unwrap();

        assert!(matches!(result, AddResult::Model { .. }));
        let stored = config.providers[0].models.get("qwen3.6-plus").unwrap();
        assert_eq!(stored.wire_apis, vec!["responses".to_string()]);
        assert_eq!(stored.desc.as_deref(), Some("Agent/终端最强"));
    }

    #[test]
    fn validate_provider_name_rejects_reserved_sentinel() {
        let config = minimal_test_config();
        let error = validate_provider_name(&config, ADD_NEW_PROVIDER_SENTINEL).unwrap_err();
        assert!(error.to_string().contains("保留的向导项"));
    }

    #[test]
    fn provider_without_endpoints_is_visible_to_all_agents() {
        let config = minimal_test_config();
        let provider = &config.providers[0];
        assert!(provider_supports_agent(&config, provider, "copilot"));
        assert!(provider_supports_agent(&config, provider, "claude"));
        assert!(provider_supports_agent(&config, provider, "codex"));
    }

    #[test]
    fn provider_with_anthropic_endpoint_matches_wire_api_compatible_agents() {
        let config = CxConfig {
            providers: vec![ProviderConfig {
                name: "Packy API".into(),
                apikey_source: None,
                models: BTreeMap::from([(
                    "claude-opus-4-7".into(),
                    ProviderModelConfig {
                        swe_pro: None,
                        lcb_pro: None,
                        hle: None,
                        desc: None,
                        context: None,
                        wire_apis: vec!["anthropic".into()],
                        agents: Vec::new(),
                        env: BTreeMap::new(),
                    },
                )]),
                endpoints: BTreeMap::from([(
                    "anthropic".into(),
                    ProviderEndpointSpec::Url("https://example.com/anthropic".into()),
                )]),
            }],
            agents: default_agent_configs(),
        };
        let provider = &config.providers[0];
        assert!(provider_supports_agent(&config, provider, "copilot"));
        assert!(provider_supports_agent(&config, provider, "claude"));
        assert!(!provider_supports_agent(&config, provider, "codex"));
    }

    #[test]
    fn model_options_group_multiple_wire_apis_under_one_model() {
        let config = multi_wire_api_test_config();
        let models = build_all_models(&config);
        let options = model_options_for_provider(&models, "copilot", "Xiaomi MIMO");

        assert_eq!(options.len(), 1);
        assert_eq!(options[0].id, "mimo-v2.5-pro");
        assert_eq!(options[0].variants.len(), 2);
        assert_eq!(options[0].variants[0].wire_api, WireApi::Anthropic);
        assert_eq!(options[0].variants[1].wire_api, WireApi::Completions);
        assert!(
            options[0]
                .formatted_row(&BTreeMap::new())
                .contains("anthropic")
        );
    }

    #[test]
    fn model_step_cycles_wire_api_and_confirms_selected_variant() {
        let config = multi_wire_api_test_config();
        let models = build_all_models(&config);
        let mut state = AppState::new(Some("copilot".into()), &config);

        assert!(state.confirm(&models).is_none());
        assert_eq!(state.step, Step::Model);
        assert!(state.current_items(&models)[0].contains("anthropic"));

        state.cycle_model_wire_api(&models, true);
        assert!(state.current_items(&models)[0].contains("completions"));

        let selection = state.confirm(&models).expect("selection should exist");
        assert_eq!(
            selection.model.expect("model should be selected").wire_api,
            WireApi::Completions
        );
    }

    #[test]
    fn legacy_provider_agents_field_is_ignored_on_parse() {
        let yaml = r#"
providers:
  - name: Legacy
    agents: [codex]
    endpoints:
      anthropic:
        url: https://example.com/anthropic
    models:
      claude-opus-4-7:
        wire_apis: [anthropic]
agents:
  - id: copilot
    bin: copilot
    wire_apis: [anthropic, responses, completions]
  - id: claude
    bin: claude
    wire_apis: [anthropic]
  - id: codex
    bin: codex
    wire_apis: [responses]
"#;

        let config: CxConfig = serde_yaml::from_str(yaml).unwrap();
        let provider = &config.providers[0];
        assert!(provider_supports_agent(&config, provider, "claude"));
        assert!(provider_supports_agent(&config, provider, "copilot"));
        assert!(!provider_supports_agent(&config, provider, "codex"));
        let serialized = serde_yaml::to_string(&config).unwrap();
        assert!(!serialized.contains("agents: [codex]"));
    }

    #[test]
    fn build_provider_endpoints_requires_at_least_one_url() {
        let error = build_provider_endpoints_from_inputs(&[
            (WireApi::Anthropic, String::new()),
            (WireApi::Responses, String::new()),
            (WireApi::Completions, String::new()),
        ])
        .unwrap_err();
        assert!(error.to_string().contains("至少填写一个"));
    }

    #[test]
    fn build_provider_endpoints_keeps_only_filled_wire_apis() {
        let endpoints = build_provider_endpoints_from_inputs(&[
            (WireApi::Anthropic, "https://example.com/anthropic".into()),
            (WireApi::Responses, String::new()),
            (WireApi::Completions, "https://example.com/v1".into()),
        ])
        .unwrap();
        assert_eq!(endpoints.len(), 2);
        assert!(endpoints.contains_key("anthropic"));
        assert!(endpoints.contains_key("completions"));
        assert!(!endpoints.contains_key("responses"));
    }

    #[test]
    fn text_input_keeps_q_and_h_as_regular_characters() {
        let mut value = String::new();
        let q = Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
        ));
        let h = Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Char('h'),
            KeyModifiers::NONE,
        ));
        assert_eq!(
            handle_text_input_event(&mut value, &q),
            TextInputAction::Changed
        );
        assert_eq!(
            handle_text_input_event(&mut value, &h),
            TextInputAction::Changed
        );
        assert_eq!(value, "qh");
    }

    #[test]
    fn text_input_appends_bracketed_paste_as_single_line() {
        let mut value = "https://".to_string();
        let paste = Event::Paste("example.com/v1\n".into());
        assert_eq!(
            handle_text_input_event(&mut value, &paste),
            TextInputAction::Changed
        );
        assert_eq!(value, "https://example.com/v1");
    }

    #[test]
    fn merge_codex_config_rewrites_provider_and_project_section() {
        let existing = r#"
model = "old-model"
model_provider = "old-provider"
approval_policy = "on-request"

[model_providers.dashscope]
name = "Old"
base_url = "https://old.example.com"
env_key = "OLD_KEY"
wire_api = "responses"

[projects."/tmp/workspace"]
trust_level = "untrusted"

[projects."/tmp/other"]
trust_level = "trusted"
"#;

        let merged = merge_codex_config(
            Some(existing),
            &test_resolved_model(
                "qwen3.6-plus",
                "https://dashscope.aliyuncs.com/v1",
                WireApi::Responses,
            ),
            Path::new("/tmp/workspace"),
        )
        .unwrap();

        assert!(merged.contains(r#"model = "qwen3.6-plus""#));
        assert!(merged.contains(r#"base_url = "https://dashscope.aliyuncs.com/v1""#));
        assert!(merged.contains(r#"[projects."/tmp/workspace"]"#));
        assert!(merged.contains(r#"trust_level = "trusted""#));
        assert!(merged.contains(r#"approval_policy = "on-request""#));
        assert!(merged.contains(r#"[projects."/tmp/other"]"#));
        assert!(!merged.contains("https://old.example.com"));
    }

    #[test]
    fn materialize_passthrough_dir_skips_overridden_entries() {
        let root = temp_test_dir("passthrough-dir");
        let real = root.join("real");
        let fake = root.join("fake");
        fs::create_dir_all(&real).unwrap();
        fs::write(real.join("keep.txt"), "ok").unwrap();
        fs::write(real.join("override.txt"), "skip").unwrap();

        materialize_passthrough_dir(&real, &fake, &["override.txt"]).unwrap();

        assert!(fake.join("keep.txt").exists());
        assert!(!fake.join("override.txt").exists());
        #[cfg(unix)]
        assert!(
            fs::symlink_metadata(fake.join("keep.txt"))
                .unwrap()
                .file_type()
                .is_symlink()
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn migrate_legacy_provider_config_moves_file() {
        let dir = temp_test_dir("provider-config-migration");
        fs::create_dir_all(&dir).unwrap();

        let current_path = dir.join(PROVIDER_CONFIG_FILE_NAME);
        let legacy_path = dir.join(LEGACY_PROVIDER_CONFIG_FILE_NAME);
        let content = "providers: []\nagents: []\n";
        fs::write(&legacy_path, content).unwrap();

        let migrated = migrate_legacy_provider_config(&current_path, &legacy_path).unwrap();
        assert!(migrated);
        assert!(!legacy_path.exists());
        assert_eq!(fs::read_to_string(&current_path).unwrap(), content);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn create_default_provider_config_writes_embedded_baseline() {
        let dir = temp_test_dir("provider-config-default");
        fs::create_dir_all(&dir).unwrap();

        let path = dir.join(PROVIDER_CONFIG_FILE_NAME);
        create_default_provider_config(&path).unwrap();

        let config = read_config_file(&path).unwrap();
        assert!(!config.providers.is_empty());
        assert!(!config.agents.is_empty());
        let packy = config
            .providers
            .iter()
            .find(|provider| provider.name == "Packy API")
            .expect("baseline should include Packy API");
        let packy_anthropic = packy
            .normalized_endpoints()
            .into_iter()
            .find(|endpoint| endpoint.wire_api == "anthropic")
            .expect("Packy API should include an anthropic endpoint");
        assert_eq!(
            CopilotAuth::from_endpoint(&packy_anthropic),
            CopilotAuth::BearerToken
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolved_agents_hide_legacy_codex_app_entry() {
        let config = CxConfig {
            providers: vec![],
            agents: vec![
                AgentConfig {
                    id: "codex".into(),
                    binary: "codex".into(),
                    args: vec![],
                    wire_apis: vec![],
                    env: BTreeMap::new(),
                },
                AgentConfig {
                    id: "codex-app".into(),
                    binary: "codex-app".into(),
                    args: vec![],
                    wire_apis: vec![],
                    env: BTreeMap::new(),
                },
            ],
        };
        let agents = resolved_agents(&config);
        assert_eq!(agents.iter().filter(|agent| agent.id == "codex").count(), 1);
        assert!(agents.iter().all(|agent| agent.id != "codex-app"));
    }

    #[test]
    fn provider_lists_end_with_add_provider() {
        let config = minimal_test_config();
        for agent in resolved_agents(&config) {
            let providers = providers_for_agent(&config, &agent.id);
            assert_eq!(
                providers.last().map(|p| p.name.as_str()),
                Some(ADD_PROVIDER_SENTINEL)
            );
        }
    }

    #[test]
    fn resolve_binary_finds_codex_cli() {
        let fake_binary = create_fake_binary("codex");
        let path = resolve_binary(&fake_binary.display().to_string()).unwrap();
        assert_eq!(path, fake_binary);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }
}
