use anyhow::{Context, Result, anyhow, bail};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use dirs::home_dir;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

const VERSION: &str = env!("CARGO_PKG_VERSION");

// ══════════════════════════════════════════════════
// 配置结构体（从 YAML 反序列化）
// ══════════════════════════════════════════════════

#[derive(Debug, Clone, Deserialize)]
struct CxConfig {
    #[serde(default)]
    providers: Vec<ProviderConfig>,
    #[serde(default)]
    agents: Vec<AgentConfig>,
}

#[derive(Debug, Clone, Deserialize)]
struct ProviderConfig {
    name: String,
    #[serde(default)]
    apikey_source: Option<String>,
    #[serde(default)]
    agents: Vec<String>,
    #[serde(default)]
    endpoints: Vec<EndpointConfig>,
}

#[derive(Debug, Clone, Deserialize)]
struct EndpointConfig {
    wire_api: String,
    url: String,
    #[serde(default)]
    models: Vec<ModelConfig>,
}

#[derive(Debug, Clone, Deserialize)]
struct ModelConfig {
    id: String,
    #[serde(default)]
    arena: Option<String>,
    #[serde(default)]
    swe_p: Option<String>,
    #[serde(default)]
    tb2: Option<String>,
    #[serde(default)]
    desc: Option<String>,
    #[serde(default)]
    agents: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct AgentConfig {
    id: String,
    binary: String,
}

// ══════════════════════════════════════════════════
// 运行时数据结构（从 config 构建，TUI 使用）
// ══════════════════════════════════════════════════

#[derive(Debug, Clone)]
struct ResolvedModel {
    id: String,
    arena: String,
    swe_p: String,
    tb2: String,
    desc: String,
    wire_api: WireApi,
    provider_name: String,
    endpoint_url: String,
    visible_agents: Vec<String>,
}

impl ResolvedModel {
    fn from_config(model: &ModelConfig, provider_name: &str, endpoint: &EndpointConfig) -> Self {
        Self {
            id: model.id.clone(),
            arena: model.arena.clone().unwrap_or_else(|| "—".to_string()),
            swe_p: model.swe_p.clone().unwrap_or_else(|| "—".to_string()),
            tb2: model.tb2.clone().unwrap_or_else(|| "—".to_string()),
            desc: model.desc.clone().unwrap_or_else(|| "".to_string()),
            wire_api: WireApi::from_str(&endpoint.wire_api),
            provider_name: provider_name.to_string(),
            endpoint_url: endpoint.url.clone(),
            visible_agents: if model.agents.is_empty() {
                provider_agents_or_all(provider_name, endpoint)
            } else {
                normalize_agent_ids(&model.agents)
            },
        }
    }

    fn formatted_row(&self) -> String {
        format!(
            "{:<24} {:>4} {:>8} {:>6}  {:<11} {}",
            self.id,
            self.arena,
            self.swe_p,
            self.tb2,
            self.wire_api.display(),
            self.desc
        )
    }

    fn supports_agent(&self, agent_id: &str) -> bool {
        let agent_id = canonical_agent_id(agent_id);
        self.visible_agents
            .iter()
            .any(|a| canonical_agent_id(a) == agent_id)
    }
}

fn provider_agents_or_all(_provider_name: &str, _endpoint: &EndpointConfig) -> Vec<String> {
    Vec::new()
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

    fn cache_value(self) -> &'static str {
        self.display()
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
            has_endpoints: !config.endpoints.is_empty(),
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
    provider: ResolvedProvider,
    model: Option<ResolvedModel>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedAgent {
    id: String,
    binary: String,
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
        agents.push(ResolvedAgent { id, binary });
    }
    agents
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

fn config_path() -> Result<PathBuf> {
    let home = home_dir().context("无法解析用户主目录")?;
    Ok(home.join(".config/cx/config.yaml"))
}

fn load_config() -> Result<CxConfig> {
    let path = config_path()?;
    if !path.exists() {
        bail!(
            "配置文件不存在: {}\n请参考 docs/cx-config-schema.yaml 创建配置文件",
            path.display()
        );
    }
    let content = fs::read_to_string(&path)
        .with_context(|| format!("读取配置文件失败: {}", path.display()))?;
    let config: CxConfig = serde_yaml::from_str(&content)
        .with_context(|| format!("解析配置文件失败: {}", path.display()))?;
    Ok(config)
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

fn build_all_models(config: &CxConfig) -> Vec<ResolvedModel> {
    let mut models = Vec::new();
    for provider in &config.providers {
        for endpoint in &provider.endpoints {
            for model in &endpoint.models {
                let mut resolved = ResolvedModel::from_config(model, &provider.name, endpoint);
                // If model's visible_agents is empty, inherit from provider or endpoint
                if resolved.visible_agents.is_empty() {
                    if provider.agents.is_empty() {
                        resolved.visible_agents =
                            resolved_agents(config).into_iter().map(|a| a.id).collect();
                    } else {
                        resolved.visible_agents = normalize_agent_ids(&provider.agents);
                    }
                }
                models.push(resolved);
            }
        }
    }
    models
}

fn apply_probe_cache(models: &mut Vec<ResolvedModel>) {
    if let Ok(cache) = load_probe_cache() {
        for model in models.iter_mut() {
            if let Some(wire_api) = cache
                .models
                .get(&model.id)
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
        .filter(|p| {
            if p.agents.is_empty() {
                true
            } else {
                p.agents.iter().any(|a| canonical_agent_id(a) == agent_id)
            }
        })
        .map(ResolvedProvider::from_config)
        .collect();

    // Append the "add provider" sentinel
    providers.push(ResolvedProvider {
        name: "+ 添加 Provider".to_string(),
        has_endpoints: false,
        apikey_source: None,
    });
    providers
}

fn models_for_provider(
    all_models: &[ResolvedModel],
    agent_id: &str,
    provider_name: &str,
) -> Vec<ResolvedModel> {
    all_models
        .iter()
        .filter(|m| m.provider_name == provider_name && m.supports_agent(agent_id))
        .cloned()
        .collect()
}

// ══════════════════════════════════════════════════
// 入口 & Invocation
// ══════════════════════════════════════════════════

pub fn run() -> Result<()> {
    let config = load_config()?;

    match parse_invocation(env::args().skip(1).collect(), &config) {
        Invocation::Help => {
            print_help();
            Ok(())
        }
        Invocation::Version => {
            println!("cx {VERSION}");
            Ok(())
        }
        Invocation::Probe { target_model } => run_probe(target_model, &config),
        Invocation::Unsupported { message } => bail!(message),
        Invocation::Launch {
            agent_hint,
            passthrough_args,
        } => run_launcher(agent_hint, &config, passthrough_args),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Invocation {
    Help,
    Version,
    Probe {
        target_model: Option<String>,
    },
    Unsupported {
        message: String,
    },
    Launch {
        agent_hint: Option<String>,
        passthrough_args: Vec<String>,
    },
}

fn parse_invocation(args: Vec<String>, config: &CxConfig) -> Invocation {
    match args.first().map(String::as_str) {
        None => Invocation::Launch {
            agent_hint: None,
            passthrough_args: Vec::new(),
        },
        Some("-h") | Some("--help") | Some("help") => Invocation::Help,
        Some("-V") | Some("--version") | Some("version") => Invocation::Version,
        Some("probe") => Invocation::Probe {
            target_model: args.get(1).cloned(),
        },
        Some("codex-app") => Invocation::Unsupported {
            message: unsupported_codex_app_message().to_string(),
        },
        Some("codex") if args.get(1).map(String::as_str) == Some("app") => {
            Invocation::Unsupported {
                message: unsupported_codex_app_message().to_string(),
            }
        }
        Some(first) => {
            if find_agent(config, first).is_some() {
                Invocation::Launch {
                    agent_hint: Some(canonical_agent_id(first).to_string()),
                    passthrough_args: args[1..].to_vec(),
                }
            } else {
                Invocation::Launch {
                    agent_hint: None,
                    passthrough_args: args,
                }
            }
        }
    }
}

fn print_help() {
    println!(
        "\
cx {VERSION}

用法：
  cx
  cx <agent> [args...]
  cx probe [model-id]
  cx --help
  cx --version

说明：
  - 运行 cx 时，总是会先进入交互式选择 Provider / Model。
  - `cx <agent> [args...]` 会跳过 agent 选择，但仍进入 Provider / Model 选择。
  - 选择完成后，剩余参数会原样透传给最终原生 CLI。
  - `cx` 不代理 `codex app` 桌面端；如需桌面端请直接运行原生 `codex app ...`。
  - `probe` 用于探测模型对 completions / responses 的支持情况。

示例：
  cx
  cx claude mcp list
  cx codex --approval-mode on-request
  cx probe
  cx probe qwen3.6-plus"
    );
}

// ══════════════════════════════════════════════════
// Launcher
// ══════════════════════════════════════════════════

fn run_launcher(
    agent_hint: Option<String>,
    config: &CxConfig,
    passthrough_args: Vec<String>,
) -> Result<()> {
    let mut all_models = build_all_models(config);
    apply_probe_cache(&mut all_models);

    let selection = run_tui(agent_hint, config, &all_models)?;

    let Some(selection) = selection else {
        return Ok(());
    };

    if selection.provider.name == "+ 添加 Provider" {
        print_add_provider_guide(&selection.agent_id);
        return Ok(());
    }

    let spec = build_launch_spec(&selection, &passthrough_args)?;

    println!();
    println!("{}", spec.summary);
    println!();

    exec_launch(spec)
}

fn print_add_provider_guide(agent_id: &str) {
    println!();
    println!("为 {agent_id} 添加 Provider：");
    println!();
    println!("1. 在 ~/.config/cx/config.yaml 的 providers 中追加新的 Provider。");
    println!("2. 如需模型选择，在 endpoint.models 中补充模型列表。");
    println!("3. 配置格式参考: docs/cx-config-schema.yaml");
    println!();
}

fn exec_launch(spec: LaunchSpec) -> Result<()> {
    let mut command = Command::new(&spec.program);
    command.args(&spec.args);
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
}

fn build_launch_spec(selection: &Selection, passthrough_args: &[String]) -> Result<LaunchSpec> {
    let program = resolve_binary(&selection.agent_binary)?;
    let mut args = Vec::new();
    let mut env = BTreeMap::new();

    let agent_id = &selection.agent_id;
    let provider = &selection.provider;

    // Default provider (no endpoints) — use agent's own default behavior
    if !provider.has_endpoints {
        match agent_id.as_str() {
            "copilot" => {
                args.extend(passthrough_args.iter().cloned());
            }
            "claude" => {
                if let Some(ref source) = provider.apikey_source {
                    let key = resolve_apikey(source)?;
                    env.insert("ANTHROPIC_API_KEY".into(), key.clone());
                    env.insert("ANTHROPIC_AUTH_TOKEN".into(), key);
                }
                args.extend(passthrough_args.iter().cloned());
            }
            "codex" => {
                if let Some(ref source) = provider.apikey_source {
                    if let Ok(value) = resolve_apikey(source) {
                        env.insert("AZURE_OPENAI_API_KEY".into(), value);
                    }
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

        let apikey = if let Some(ref source) = provider.apikey_source {
            resolve_apikey(source)?
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
                env.insert("COPILOT_PROVIDER_TYPE".into(), "openai".into());
                env.insert("COPILOT_PROVIDER_API_KEY".into(), apikey);
                env.insert("COPILOT_MODEL".into(), model.id.clone());
                env.insert(
                    "COPILOT_PROVIDER_WIRE_API".into(),
                    model.wire_api.launch_value()?.to_string(),
                );
                args.extend(passthrough_args.iter().cloned());
            }
            "claude" => {
                env.insert("ANTHROPIC_BASE_URL".into(), model.endpoint_url.clone());
                env.insert("ANTHROPIC_API_KEY".into(), apikey);
                env.insert("ANTHROPIC_MODEL".into(), model.id.clone());
                args.extend(passthrough_args.iter().cloned());
            }
            "codex" => {
                env.insert("DASHSCOPE_API_KEY".into(), apikey);
                args.extend([
                    "-c".to_string(),
                    r#"model_provider="dashscope""#.to_string(),
                    "-c".to_string(),
                    format!(r#"model="{}""#, model.id),
                ]);
                args.extend(passthrough_args.iter().cloned());
            }
            _ => {
                // Generic fallback: just pass through
                args.extend(passthrough_args.iter().cloned());
            }
        }
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
    })
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

fn save_probe_cache(cache: &ProbeCache) -> Result<()> {
    let path = probe_cache_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("创建 probe 缓存目录失败: {}", parent.display()))?;
    }
    fs::write(&path, serde_json::to_vec_pretty(cache)?)
        .with_context(|| format!("写入 probe 缓存失败: {}", path.display()))?;
    println!("\n缓存已保存到 {}", path.display());
    Ok(())
}

fn run_probe(target_model: Option<String>, config: &CxConfig) -> Result<()> {
    // Find the first provider with endpoints that has a completions/responses URL and apikey
    let probe_provider = config
        .providers
        .iter()
        .find(|p| !p.endpoints.is_empty() && p.apikey_source.is_some())
        .context("没有可探测的 Provider（需要至少一个有 endpoint 和 apikey_source 的 Provider）")?;

    let apikey = resolve_apikey(probe_provider.apikey_source.as_ref().unwrap())?;

    // Use the first endpoint's URL as the probe base URL
    let probe_base_url = &probe_provider.endpoints[0].url;

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("初始化 HTTP 客户端失败")?;

    let mut all_models = build_all_models(config);
    if let Some(ref target) = target_model {
        all_models.retain(|model| model.id == *target);
        if all_models.is_empty() {
            let available_ids = build_all_models(config)
                .iter()
                .map(|m| m.id.clone())
                .collect::<Vec<_>>()
                .join(", ");
            bail!("未知模型 `{target}`，可用模型：{available_ids}");
        }
    }

    println!("开始探测 {} 个模型...\n", all_models.len());

    let mut cache = ProbeCache {
        timestamp: chrono_like_timestamp(),
        models: BTreeMap::new(),
    };

    for model in &mut all_models {
        println!("探测 {}...", model.id);
        let result = probe_model(&client, &apikey, &model.id, probe_base_url)?;

        let completions_status = if result.completions.ok {
            "✅ completions"
        } else {
            "❌ completions"
        };
        let responses_status = if result.responses.ok {
            "✅ responses"
        } else {
            "❌ responses"
        };
        println!("  {} | {}", completions_status, responses_status);

        if !result.completions.ok {
            if let Some(error) = &result.completions.error {
                println!("  completions: {error}");
            }
        }
        if !result.responses.ok {
            if let Some(error) = &result.responses.error {
                println!("  responses: {error}");
            }
        }

        model.wire_api = if result.responses.ok {
            WireApi::Responses
        } else if result.completions.ok {
            WireApi::Completions
        } else {
            WireApi::Unavailable
        };

        cache
            .models
            .insert(model.id.clone(), model.wire_api.cache_value().to_string());
        println!();
    }

    save_probe_cache(&cache)?;
    Ok(())
}

fn chrono_like_timestamp() -> String {
    let now = std::time::SystemTime::now();
    match now.duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => format!("{}", duration.as_secs()),
        Err(_) => "0".to_string(),
    }
}

#[derive(Debug)]
struct ProbeResult {
    completions: EndpointResult,
    responses: EndpointResult,
}

#[derive(Debug)]
struct EndpointResult {
    ok: bool,
    error: Option<String>,
}

fn probe_model(
    client: &reqwest::blocking::Client,
    api_key: &str,
    model_id: &str,
    base_url: &str,
) -> Result<ProbeResult> {
    let completions = probe_endpoint(
        client,
        api_key,
        base_url,
        "chat/completions",
        json!({
            "model": model_id,
            "messages": [{"role": "user", "content": "Hi"}],
            "max_tokens": 5,
            "tools": [{
                "type": "function",
                "function": {
                    "name": "test",
                    "description": "test",
                    "parameters": {
                        "type": "object",
                        "properties": {"x": {"type": "string"}},
                        "required": ["x"]
                    }
                }
            }]
        }),
    )?;

    let responses = probe_endpoint(
        client,
        api_key,
        base_url,
        "responses",
        json!({
            "model": model_id,
            "input": [{"role": "user", "content": "Hi"}],
            "max_output_tokens": 5,
        }),
    )?;

    Ok(ProbeResult {
        completions,
        responses,
    })
}

fn probe_endpoint(
    client: &reqwest::blocking::Client,
    api_key: &str,
    base_url: &str,
    path: &str,
    body: serde_json::Value,
) -> Result<EndpointResult> {
    let url = format!("{base_url}/{path}");
    let response = client
        .post(&url)
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .context("调用探测 API 失败")?;

    let status = response.status();
    let text = response.text().context("读取探测 API 响应失败")?;
    let json: Option<serde_json::Value> = serde_json::from_str(&text).ok();

    if status.is_success() {
        if let Some(json) = json {
            if let Some(error) = json.get("error") {
                let message = error
                    .get("message")
                    .and_then(|value| value.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                return Ok(EndpointResult {
                    ok: false,
                    error: Some(message),
                });
            }
        }
        return Ok(EndpointResult {
            ok: true,
            error: None,
        });
    }

    let error = json
        .and_then(|json| {
            json.get("error")
                .and_then(|value| value.get("message"))
                .and_then(|value| value.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| format!("HTTP {}", status.as_u16()));

    Ok(EndpointResult {
        ok: false,
        error: Some(error),
    })
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
                .current_models(models)
                .iter()
                .map(ResolvedModel::formatted_row)
                .collect(),
        }
    }

    fn current_models(&self, models: &[ResolvedModel]) -> Vec<ResolvedModel> {
        let providers = providers_for_agent(&self.config, &self.selected_agent_id);
        let provider = &providers[self.provider_index];
        models_for_provider(models, &self.selected_agent_id, &provider.name)
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
                        provider,
                        model: None,
                    })
                }
            }
            Step::Model => {
                let providers = providers_for_agent(&self.config, &self.selected_agent_id);
                let provider = providers[self.provider_index].clone();
                let available = self.current_models(models);
                if self.model_index >= available.len() {
                    return None;
                }
                let agent = find_agent(&self.config, &self.selected_agent_id).unwrap();
                Some(Selection {
                    agent_id: agent.id.clone(),
                    agent_binary: agent.binary.clone(),
                    provider,
                    model: Some(available[self.model_index].clone()),
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
    execute!(stdout, EnterAlternateScreen).context("进入备用屏幕失败")?;
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
                KeyCode::Enter => {
                    if let Some(selection) = state.confirm(models) {
                        return Ok(Some(selection));
                    }
                }
                KeyCode::Esc | KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => {
                    if state.go_back() {
                        return Ok(None);
                    }
                }
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
        let _ = execute!(stdout, LeaveAlternateScreen);
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

    let footer = Paragraph::new("↑/↓ 或 j/k 移动  ·  Enter 确认  ·  Esc/Backspace 返回  ·  q 退出")
        .style(Style::default().fg(Color::DarkGray))
        .wrap(Wrap { trim: true });
    frame.render_widget(footer, layout[3]);
}

fn current_title(state: &AppState) -> &'static str {
    match state.step {
        Step::Agent => "选择 Agent",
        Step::Provider => "选择 Provider",
        Step::Model => "选择 Model",
    }
}

fn current_prompt(state: &AppState) -> String {
    match state.step {
        Step::Agent => "选择 Agent".to_string(),
        Step::Provider => "选择 Provider".to_string(),
        Step::Model => "选择 Model".to_string(),
    }
}

// ══════════════════════════════════════════════════
// Tests
// ══════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> CxConfig {
        let yaml =
            std::fs::read_to_string(dirs::home_dir().unwrap().join(".config/cx/config.yaml"))
                .unwrap();
        serde_yaml::from_str::<CxConfig>(&yaml).unwrap()
    }

    #[test]
    fn config_loads_successfully() {
        let config = test_config();
        assert!(!config.providers.is_empty());
        assert!(!config.agents.is_empty());
    }

    #[test]
    fn parse_no_args() {
        let config = test_config();
        assert_eq!(
            parse_invocation(vec![], &config),
            Invocation::Launch {
                agent_hint: None,
                passthrough_args: vec![],
            }
        );
    }

    #[test]
    fn parse_agent_with_passthrough() {
        let config = test_config();
        assert_eq!(
            parse_invocation(vec!["claude".into(), "mcp".into(), "list".into()], &config),
            Invocation::Launch {
                agent_hint: Some("claude".into()),
                passthrough_args: vec!["mcp".into(), "list".into()],
            }
        );
    }

    #[test]
    fn parse_passthrough_without_agent_hint() {
        let config = test_config();
        assert_eq!(
            parse_invocation(vec!["mcp".into(), "list".into()], &config),
            Invocation::Launch {
                agent_hint: None,
                passthrough_args: vec!["mcp".into(), "list".into()],
            }
        );
    }

    #[test]
    fn parse_probe() {
        let config = test_config();
        assert_eq!(
            parse_invocation(vec!["probe".into(), "glm-5".into()], &config),
            Invocation::Probe {
                target_model: Some("glm-5".into()),
            }
        );
    }

    #[test]
    fn parse_codex_app_is_rejected() {
        let config = test_config();
        let invocation = parse_invocation(vec!["codex".into(), "app".into(), ".".into()], &config);
        assert!(
            matches!(invocation, Invocation::Unsupported { .. }),
            "expected codex app to be rejected, got {invocation:?}"
        );
    }

    #[test]
    fn parse_legacy_codex_app_alias_is_rejected() {
        let config = test_config();
        let invocation = parse_invocation(vec!["codex-app".into(), ".".into()], &config);
        assert!(
            matches!(invocation, Invocation::Unsupported { .. }),
            "expected codex-app alias to be rejected, got {invocation:?}"
        );
    }

    #[test]
    fn parse_non_desktop_codex_subcommand_still_passes_through() {
        let config = test_config();
        assert_eq!(
            parse_invocation(vec!["codex".into(), "exec".into(), "app".into()], &config),
            Invocation::Launch {
                agent_hint: Some("codex".into()),
                passthrough_args: vec!["exec".into(), "app".into()],
            }
        );
    }

    #[test]
    fn codex_mcp_passthrough_stays_raw_without_endpoints() {
        let selection = Selection {
            agent_id: "codex".into(),
            agent_binary: "codex".into(),
            provider: ResolvedProvider {
                name: "Codex Default".into(),
                has_endpoints: false,
                apikey_source: None,
            },
            model: None,
        };

        let spec = build_launch_spec(&selection, &["mcp".into(), "serve".into()]).unwrap();
        assert_eq!(spec.args, vec!["mcp".to_string(), "serve".to_string()]);
        assert!(!spec.args.iter().any(|arg| arg.starts_with("--cwd")));
    }

    #[test]
    fn codex_mcp_passthrough_never_injects_cwd_with_endpoint_provider() {
        let selection = Selection {
            agent_id: "codex".into(),
            agent_binary: "codex".into(),
            provider: ResolvedProvider {
                name: "DashScope".into(),
                has_endpoints: true,
                apikey_source: Some("literal:test-key".into()),
            },
            model: Some(ResolvedModel {
                id: "qwen3-coder-plus".into(),
                arena: "—".into(),
                swe_p: "—".into(),
                tb2: "—".into(),
                desc: String::new(),
                wire_api: WireApi::Responses,
                provider_name: "DashScope".into(),
                endpoint_url: "https://dashscope.aliyuncs.com/compatible-mode/v1".into(),
                visible_agents: vec!["codex".into()],
            }),
        };

        let spec = build_launch_spec(&selection, &["mcp".into(), "list".into()]).unwrap();
        assert_eq!(
            spec.args,
            vec![
                "-c".to_string(),
                r#"model_provider="dashscope""#.to_string(),
                "-c".to_string(),
                r#"model="qwen3-coder-plus""#.to_string(),
                "mcp".to_string(),
                "list".to_string(),
            ]
        );
        assert!(!spec.args.iter().any(|arg| arg.starts_with("--cwd")));
    }

    #[test]
    fn claude_dashscope_hides_minimax_m27() {
        let config = test_config();
        let all_models = build_all_models(&config);
        let models = models_for_provider(&all_models, "claude", "百炼 Coding Plan");
        assert!(
            models.iter().all(|model| model.id != "MiniMax-M2.7"),
            "Claude should not offer MiniMax-M2.7 on DashScope"
        );
    }

    #[test]
    fn codex_dashscope_keeps_minimax_m27() {
        let config = test_config();
        let all_models = build_all_models(&config);
        let models = models_for_provider(&all_models, "codex", "百炼 Coding Plan");
        assert!(
            models.iter().any(|model| model.id == "MiniMax-M2.7"),
            "Codex should offer MiniMax-M2.7 on DashScope"
        );
    }

    #[test]
    fn provider_lists_end_with_add_provider() {
        let config = test_config();
        for agent in &config.agents {
            let providers = providers_for_agent(&config, &agent.id);
            assert_eq!(
                providers.last().map(|p| p.name.as_str()),
                Some("+ 添加 Provider")
            );
        }
    }

    #[test]
    fn copilot_only_sees_copilot_providers() {
        let config = test_config();
        let providers = providers_for_agent(&config, "copilot");
        // Should include: 百炼 Coding Plan (all agents), GitHub Copilot Plan (copilot only)
        assert!(providers.iter().any(|p| p.name == "百炼 Coding Plan"));
        assert!(providers.iter().any(|p| p.name == "GitHub Copilot Plan"));
        // Should NOT include: Packy API (claude only), Azure OpenAI (codex only)
        assert!(
            providers
                .iter()
                .all(|p| p.name != "Packy API — Claude Opus 4.6")
        );
        assert!(providers.iter().all(|p| p.name != "Azure OpenAI"));
    }

    #[test]
    fn resolved_agents_hide_legacy_codex_app_entry() {
        let config = test_config();
        let agents = resolved_agents(&config);
        assert_eq!(agents.iter().filter(|agent| agent.id == "codex").count(), 1);
        assert!(agents.iter().all(|agent| agent.id != "codex-app"));
    }

    #[test]
    fn codex_alias_and_native_agent_see_same_providers() {
        let config = test_config();
        let codex = providers_for_agent(&config, "codex");
        let legacy = providers_for_agent(&config, "codex-app");
        assert_eq!(
            codex
                .iter()
                .map(|provider| provider.name.clone())
                .collect::<Vec<_>>(),
            legacy
                .iter()
                .map(|provider| provider.name.clone())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn codex_alias_and_native_agent_see_same_models() {
        let config = test_config();
        let all_models = build_all_models(&config);
        let codex = models_for_provider(&all_models, "codex", "百炼 Coding Plan");
        let legacy = models_for_provider(&all_models, "codex-app", "百炼 Coding Plan");
        assert_eq!(
            codex
                .iter()
                .map(|model| model.id.clone())
                .collect::<Vec<_>>(),
            legacy
                .iter()
                .map(|model| model.id.clone())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn resolve_binary_finds_codex_cli() {
        let path = resolve_binary("codex").unwrap();
        assert!(path.ends_with("codex"));
    }
}
