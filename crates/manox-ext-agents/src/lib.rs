//! manox-ext-agents — library half of the cx agent launcher.
//!
//! This crate provides the programmatic agent launch API, session management,
//! IPC relay, and provider/model selection logic, extracted from the `cx` crate.
//! It does NOT depend on CLI crates (clap, crossterm, ratatui, signal-hook),
//! so it can be used by `manox` and `agent-ui` without pulling in terminal
//! dependencies.

use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use dirs::home_dir;
use rand::RngCore;
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::{self};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
#[cfg(windows)]
use std::os::windows::fs::{symlink_dir, symlink_file};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use manox_providers::{
    AgentConfig, ChatGptAppSettings, CopilotAuth, CxConfig, PROVIDER_CONFIG_FILE_NAME,
    ProviderConfig, ResolvedAgent, ResolvedModel, VsCodeAppSettings, VsCodeExtensionBlock, WireApi,
    active_provider_config_path, canonical_agent_id, context_window_from_suffix, cx_state_dir,
    effective_agents_for_model, read_config_file, resolve_apikey, resolved_agents,
};

pub mod api;
pub mod chatgpt_app;
pub mod relay;
pub mod send;
pub mod session;
pub use api::{Agent, AgentBuilder, SessionHandle, SessionResult};
pub mod vscode_app;
pub mod warp;

const LAUNCH_HOME_DIR_NAME: &str = "cx-launch-homes";
const LAUNCH_HOME_TTL_SECS: u64 = 60 * 60 * 24;
const ADD_PROVIDER_SENTINEL: &str = "+ 添加 Provider";
const DEFAULT_PROVIDER_CONFIG_YAML: &str = include_str!("../config/providers.default.yaml");

// ══════════════════════════════════════════════════
// File helpers
// ══════════════════════════════════════════════════

pub fn write_string_atomic(path: &Path, content: &str) -> Result<()> {
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

pub fn ensure_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("创建目录失败: {}", path.display()))?;
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("设置目录权限失败: {}", path.display()))?;
    Ok(())
}

pub fn write_private_file(path: &Path, content: &str) -> Result<()> {
    write_string_atomic(path, content)?;
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("设置文件权限失败: {}", path.display()))?;
    Ok(())
}

/// 若 `dst` 已存在，在重建符号链接前先移除，保证 `materialize_passthrough_dir` 幂等。
pub fn remove_existing_entry(dst: &Path) {
    let Ok(meta) = fs::symlink_metadata(dst) else {
        return;
    };
    let ft = meta.file_type();
    if ft.is_dir() && !ft.is_symlink() {
        return;
    }
    #[cfg(unix)]
    {
        let _ = fs::remove_file(dst);
    }
    #[cfg(windows)]
    {
        let _ = if ft.is_dir() {
            fs::remove_dir(dst)
        } else {
            fs::remove_file(dst)
        };
    }
}

#[cfg(unix)]
pub fn symlink_path(src: &Path, dst: &Path) -> Result<()> {
    remove_existing_entry(dst);
    if dst.exists() {
        return Ok(());
    }
    std::os::unix::fs::symlink(src, dst)
        .with_context(|| format!("创建符号链接失败: {} -> {}", dst.display(), src.display()))
}

#[cfg(windows)]
pub fn symlink_path(src: &Path, dst: &Path) -> Result<()> {
    remove_existing_entry(dst);
    if dst.exists() {
        return Ok(());
    }
    let result = if src.is_dir() {
        symlink_dir(src, dst)
    } else {
        symlink_file(src, dst)
    };
    result.with_context(|| format!("创建符号链接失败: {} -> {}", dst.display(), src.display()))
}

// ══════════════════════════════════════════════════
// Launch home / FHS sandbox
// ══════════════════════════════════════════════════

pub fn launch_homes_root() -> PathBuf {
    env::temp_dir().join(LAUNCH_HOME_DIR_NAME)
}

pub fn sweep_old_launch_homes() -> Result<()> {
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

pub fn create_launch_home(agent_id: &str) -> Result<PathBuf> {
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

pub fn mirror_home_entries(real_home: &Path, fake_home: &Path, excluded: &[&str]) -> Result<()> {
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

pub fn materialize_passthrough_dir(
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

pub fn toml_basic_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

pub fn provider_config_key(provider_name: &str) -> String {
    let slug: String = provider_name
        .chars()
        .filter_map(|c| {
            if c.is_ascii_alphanumeric() {
                Some(c.to_ascii_lowercase())
            } else if c == '-' || c == '_' {
                Some(c)
            } else {
                None
            }
        })
        .collect();
    if slug.is_empty() {
        "custom".to_string()
    } else {
        slug
    }
}

pub fn env_key_for_apikey_source(source: Option<&str>) -> String {
    if let Some(s) = source
        && let Some(rest) = s
            .strip_prefix("keychain:")
            .or_else(|| s.strip_prefix("env:"))
    {
        let v = rest.trim();
        if !v.is_empty() {
            return v.to_string();
        }
    }
    "CX_PROVIDER_KEY".to_string()
}

pub fn extract_reasoning_effort(existing: Option<&str>) -> Option<String> {
    let existing = existing?;
    for line in existing.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            break;
        }
        if let Some(after) = trimmed.strip_prefix("model_reasoning_effort") {
            let after = after.trim_start();
            if let Some(rhs) = after.strip_prefix('=') {
                let v = rhs.trim().trim_matches('"');
                if !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
    }
    None
}

pub fn codex_wire_api_str(wire_api: WireApi) -> Result<&'static str> {
    match wire_api {
        WireApi::Responses => Ok("responses"),
        WireApi::Completions => Ok("chat_completions"),
        WireApi::Anthropic => Ok("anthropic_messages"),
        WireApi::Unavailable => {
            bail!("该模型当前被标记为 unavailable，请先运行 `cx probe` 更新探测结果")
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn merge_codex_config(
    existing: Option<&str>,
    model: &ResolvedModel,
    workspace_root: &Path,
    wire_api: WireApi,
    provider_key: &str,
    provider_name: &str,
    env_key: &str,
    api_model_id: &str,
    context_window: Option<i64>,
    model_catalog_json: Option<&str>,
) -> Result<String> {
    let project_section = format!(
        "[projects.{}]",
        toml_basic_string(&workspace_root.to_string_lossy())
    );
    let reasoning_effort = extract_reasoning_effort(existing).unwrap_or_else(|| "high".to_string());
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

            if trimmed.starts_with("[model_providers.") || trimmed == project_section {
                skipping_section = true;
                continue;
            }

            if !trimmed.starts_with('[')
                && (trimmed.starts_with("model =")
                    || trimmed.starts_with("model_provider =")
                    || trimmed.starts_with("model_reasoning_effort =")
                    || (context_window.is_some() && trimmed.starts_with("model_context_window ="))
                    || (model_catalog_json.is_some()
                        && trimmed.starts_with("model_catalog_json =")))
            {
                continue;
            }

            retained.push(line.to_string());
        }
    }

    let wire_api_str = codex_wire_api_str(wire_api)?;
    let context_window_line = context_window
        .map(|n| format!("model_context_window = {n}\n"))
        .unwrap_or_default();
    let model_catalog_json_line = model_catalog_json
        .map(|p| format!("model_catalog_json = {}\n", toml_basic_string(p)))
        .unwrap_or_default();
    let mut rendered = format!(
        "model = {}\nmodel_provider = {}\nmodel_reasoning_effort = {}\n{}{}[model_providers.{}]\nname = {}\nbase_url = {}\nenv_key = {}\nwire_api = {}\n\n{}{}\ntrust_level = \"trusted\"\n",
        toml_basic_string(api_model_id),
        toml_basic_string(provider_key),
        toml_basic_string(&reasoning_effort),
        context_window_line,
        model_catalog_json_line,
        toml_basic_string(provider_key),
        toml_basic_string(provider_name),
        toml_basic_string(&model.endpoint_url),
        toml_basic_string(env_key),
        toml_basic_string(wire_api_str),
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

/// 解析 model id 末尾的上下文窗口后缀（cx 约定，如 `glm-5.2[1m]`、`model[200k]`、`model[1m123k]`）。
/// 返回 (发给 provider/agent 的 base id, Option<上下文 token 数>)。
/// `[1m]` → 1_000_000；`[200k]` → 200_000；无后缀则 base = 原 id、hint = None。
/// 不匹配的尾缀（如 `model[1mm]`）原样保留。
pub(crate) fn parse_model_context_suffix(model_id: &str) -> (&str, Option<i64>) {
    match context_window_from_suffix(model_id) {
        Some(tokens) => {
            let open = model_id.rfind('[').unwrap();
            (&model_id[..open], Some(tokens as i64))
        }
        None => (model_id, None),
    }
}

#[allow(unused)]
pub fn inject_supports_websockets(config: &str, supports: bool) -> String {
    let marker = "\nwire_api = ";
    let Some(pos) = config.find(marker) else {
        return config.to_string();
    };
    let rest = &config[pos + marker.len()..];
    let Some(eol) = rest.find('\n') else {
        return config.to_string();
    };
    let insert_at = pos + marker.len() + eol + 1;
    let mut out = String::with_capacity(config.len() + 32);
    out.push_str(&config[..insert_at]);
    out.push_str(&format!("supports_websockets = {supports}\n"));
    out.push_str(&config[insert_at..]);
    out
}

pub fn prepare_codex_launch_home(
    model: &ResolvedModel,
    provider: &ResolvedProvider,
    apikey: String,
    env: &mut BTreeMap<String, String>,
    wire_api: WireApi,
) -> Result<()> {
    let real_home = home_dir().context("无法解析用户主目录")?;
    let fake_home = create_launch_home("codex")?;
    mirror_home_entries(&real_home, &fake_home, &[".codex"])?;

    let real_codex_dir = real_home.join(".codex");
    let fake_codex_dir = fake_home.join(".codex");
    materialize_passthrough_dir(&real_codex_dir, &fake_codex_dir, &["config.toml"])?;

    let provider_key = provider_config_key(&provider.name);
    let env_key = env_key_for_apikey_source(provider.apikey_source.as_deref());
    let existing_config = fs::read_to_string(real_codex_dir.join("config.toml")).ok();
    let (api_model_id, context_window) = parse_model_context_suffix(&model.id);
    let merged_config = merge_codex_config(
        existing_config.as_deref(),
        model,
        &env::current_dir()?,
        wire_api,
        &provider_key,
        &provider.name,
        &env_key,
        api_model_id,
        context_window,
        None,
    )?;
    write_private_file(&fake_codex_dir.join("config.toml"), &merged_config)?;

    env.insert(env_key.clone(), apikey);
    env.insert("HOME".into(), fake_home.display().to_string());
    env.insert(
        "XDG_CONFIG_HOME".into(),
        fake_home.join(".config").display().to_string(),
    );
    env.insert("CODEX_HOME".into(), fake_codex_dir.display().to_string());
    Ok(())
}

/// 为 Codex Desktop App 准备注入配置。
#[allow(unused)]
pub fn prepare_chatgpt_launch_home_for_app(
    model: &ResolvedModel,
    provider: &ResolvedProvider,
    wire_api: WireApi,
    injected_models: &[ResolvedModel],
    chatgpt_settings: &ChatGptAppSettings,
    provider_display_name: &str,
) -> Result<ChatGptAppPrepared> {
    let real_home = home_dir().context("无法解析用户主目录")?;
    let codex_dir = cx_state_dir()?.join(".codex");
    let real_codex_dir = real_home.join(".codex");

    if !codex_dir.exists() {
        std::fs::create_dir_all(&codex_dir)?;
    }

    materialize_passthrough_dir(&real_codex_dir, &codex_dir, &["config.toml"])?;

    let provider_key = provider_config_key(&provider.name);
    let env_key = env_key_for_apikey_source(provider.apikey_source.as_deref());
    let existing_config = fs::read_to_string(real_codex_dir.join("config.toml")).ok();
    let reasoning_effort =
        extract_reasoning_effort(existing_config.as_deref()).unwrap_or_else(|| "high".to_string());
    let (api_model_id, context_window) = parse_model_context_suffix(&model.id);
    let catalog_path = codex_dir.join("model-catalog.json");
    let catalog = chatgpt_app::catalog::build_model_catalog(injected_models);
    let has_catalog_entries = catalog["models"]
        .as_array()
        .map(|models| !models.is_empty())
        .unwrap_or(false);
    let suffix_count = catalog["models"].as_array().map(|a| a.len()).unwrap_or(0);
    println!(
        "[cx] 注入 {} 个模型（其中 {} 个有上下文后缀）",
        injected_models.len(),
        suffix_count
    );
    if !has_catalog_entries {
        eprintln!("[cx] 警告: 无上下文后缀模型，所有模型将走引擎 fallback（258k）");
    }
    let model_catalog_json = if has_catalog_entries {
        write_private_file(&catalog_path, &serde_json::to_string_pretty(&catalog)?)?;
        println!("[cx] 注入模型目录: {}", catalog_path.display());
        Some(catalog_path.to_string_lossy().to_string())
    } else {
        None
    };
    let merged_config = merge_codex_config(
        existing_config.as_deref(),
        model,
        &env::current_dir()?,
        wire_api,
        &provider_key,
        provider_display_name,
        &env_key,
        api_model_id,
        context_window,
        model_catalog_json.as_deref(),
    )?;
    let merged_config = inject_supports_websockets(
        &merged_config,
        chatgpt_settings.supports_websockets.unwrap_or(false),
    );
    write_private_file(&codex_dir.join("config.toml"), &merged_config)?;
    println!("[cx] 注入配置: {}", codex_dir.join("config.toml").display());

    Ok(ChatGptAppPrepared {
        codex_home: codex_dir,
        env_key,
        reasoning_effort,
        custom_env: chatgpt_settings.env.clone(),
    })
}

/// `prepare_chatgpt_launch_home_for_app` 的产物。
pub struct ChatGptAppPrepared {
    pub codex_home: PathBuf,
    pub env_key: String,
    pub reasoning_effort: String,
    pub custom_env: BTreeMap<String, String>,
}

// ══════════════════════════════════════════════════
// Config / selection helpers
// ══════════════════════════════════════════════════

/// Normalize a CLI-supplied agent name: case-insensitive, with alias collapsing.
pub fn canonicalize_agent_name(input: &str) -> String {
    match input.to_lowercase().as_str() {
        "codex_app" | "codexapp" | "codex.app" | "chatgpt_app" | "chatgptapp" | "chatgpt.app" => {
            "ChatGPT.app".into()
        }
        "vscode" | "vs-code" | "vs_code" | "vs code" | "vscode.app" => "VS Code".into(),
        other => other.into(),
    }
}

pub fn find_agent(config: &CxConfig, agent_id: &str) -> Option<ResolvedAgent> {
    let normalized = canonicalize_agent_name(agent_id);
    let agent_id = canonical_agent_id(&normalized);
    resolved_agents(config)
        .into_iter()
        .find(|agent| agent.id == agent_id)
}

pub fn wire_api_launch_value(wire_api: WireApi) -> Result<&'static str> {
    match wire_api {
        WireApi::Responses => Ok("responses"),
        WireApi::Completions => Ok("completions"),
        WireApi::Anthropic => Ok("anthropic"),
        WireApi::Unavailable => {
            bail!("该模型当前被标记为 unavailable，请先运行 `cx probe` 更新探测结果")
        }
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedProvider {
    pub name: String,
    pub has_endpoints: bool,
    pub apikey_source: Option<String>,
    pub env: BTreeMap<String, String>,
}

impl ResolvedProvider {
    pub fn from_config(config: &ProviderConfig) -> Self {
        Self {
            name: config.name.clone(),
            has_endpoints: config.has_endpoints(),
            apikey_source: config.apikey_source.clone(),
            env: config.env.clone(),
        }
    }

    pub fn requires_model(&self) -> bool {
        self.has_endpoints
    }
}

#[derive(Debug, Clone)]
pub struct Selection {
    pub agent_id: String,
    pub agent_binary: String,
    pub agent_args: Vec<String>,
    pub agent_env: BTreeMap<String, String>,
    pub selected_wire_api: WireApi,
    pub provider: ResolvedProvider,
    pub model: Option<ResolvedModel>,
    /// 仅 ChatGPT.app 等注入型 agent 使用。
    pub injected_models: Vec<ResolvedModel>,
}

pub fn default_agent_configs() -> Vec<AgentConfig> {
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

pub fn available_agents_for_add(config: &CxConfig) -> Vec<ResolvedAgent> {
    let agents = resolved_agents(config);
    if !agents.is_empty() {
        return agents.into_iter().filter(|a| !a.hidden).collect();
    }

    resolved_agents(&CxConfig {
        providers: Vec::new(),
        agents: default_agent_configs(),
        chatgpt_app: None,
        vscode_app: None,
        subagents: Default::default(),
    })
    .into_iter()
    .filter(|a| !a.hidden)
    .collect()
}

pub fn create_default_provider_config(path: &Path) -> Result<()> {
    write_string_atomic(path, DEFAULT_PROVIDER_CONFIG_YAML)
        .with_context(|| format!("创建默认 Provider 配置失败: {}", path.display()))?;
    eprintln!("未找到 Provider 配置，已按基线创建: {}", path.display());
    Ok(())
}

pub fn load_config() -> Result<CxConfig> {
    let path = active_provider_config_path()?;
    if !path.exists() {
        create_default_provider_config(&path)?;
    }
    read_config_file(&path)
}

pub fn load_config_for_add() -> Result<(CxConfig, PathBuf)> {
    let path = active_provider_config_path()?;
    let config = if path.exists() {
        read_config_file(&path)?
    } else {
        CxConfig {
            providers: Vec::new(),
            agents: default_agent_configs(),
            chatgpt_app: None,
            vscode_app: None,
            subagents: Default::default(),
        }
    };
    Ok((config, path))
}

pub fn save_config(path: &Path, config: &CxConfig) -> Result<()> {
    let yaml = serde_yaml::to_string(config).context("序列化配置失败")?;
    write_string_atomic(path, &yaml)
}

pub fn current_unix_secs() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("系统时间早于 Unix Epoch")?
        .as_secs())
}

pub fn random_urlsafe(bytes: usize) -> String {
    let mut raw = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut raw);
    URL_SAFE_NO_PAD.encode(raw)
}

pub fn build_all_models(config: &CxConfig) -> Vec<ResolvedModel> {
    let mut models = Vec::new();
    for provider in &config.providers {
        match resolved_models_for_provider(config, provider) {
            Ok(mut resolved) => models.append(&mut resolved),
            Err(e) => {
                eprintln!(
                    "警告: 获取 Provider `{}` 的 models 失败: {e}",
                    provider.name
                );
            }
        }
    }
    models
}

pub fn resolved_models_for_provider(
    config: &CxConfig,
    provider: &ProviderConfig,
) -> Result<Vec<ResolvedModel>> {
    let fetched = probe::runtime().block_on(provider.list_models())?;
    let mut models = Vec::new();
    for endpoint in provider.normalized_endpoints_resolved(&fetched) {
        for model in &endpoint.models {
            models.push(ResolvedModel::from_config(
                config, provider, &endpoint, model,
            ));
        }
    }
    Ok(models)
}

pub fn provider_supports_agent(
    config: &CxConfig,
    provider: &ProviderConfig,
    agent_id: &str,
) -> bool {
    let agent_id = canonical_agent_id(agent_id);
    if !provider.has_endpoints() {
        return true;
    }
    if provider.is_remote_models() {
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

pub fn apply_probe_cache(models: &mut [ResolvedModel]) {
    let db_path = match cx_state_dir().map(|d| d.join("cx.db")) {
        Ok(p) => p,
        Err(_) => return,
    };

    let conn = match rusqlite::Connection::open(&db_path) {
        Ok(c) => c,
        Err(_) => return,
    };

    for model in models.iter_mut() {
        match probe::db::get_available_wire_api(&conn, &model.provider_name, &model.id) {
            Ok(Some(wire_api)) => {
                model.wire_api = wire_api;
            }
            Ok(None) => {
                model.wire_api = WireApi::Unavailable;
            }
            Err(_) => {}
        }
    }
}

pub fn providers_for_agent(config: &CxConfig, agent_id: &str) -> Vec<ResolvedProvider> {
    let agent_id = canonical_agent_id(agent_id);
    let mut providers: Vec<ResolvedProvider> = config
        .providers
        .iter()
        .filter(|provider| provider_supports_agent(config, provider, agent_id))
        .map(ResolvedProvider::from_config)
        .collect();

    providers.push(ResolvedProvider {
        name: ADD_PROVIDER_SENTINEL.to_string(),
        has_endpoints: false,
        apikey_source: None,
        env: BTreeMap::new(),
    });
    providers
}

pub fn model_options_for_provider(
    all_models: &[ResolvedModel],
    agent_id: &str,
    provider_name: &str,
) -> Vec<ModelOption> {
    let mut grouped: Vec<Vec<ResolvedModel>> = Vec::new();
    let mut indexes_by_id: BTreeMap<String, usize> = BTreeMap::new();

    for model in all_models
        .iter()
        .filter(|m| m.provider_name == provider_name && resolved_model_supports_agent(m, agent_id))
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
// ModelOption and related types
// ══════════════════════════════════════════════════

#[derive(Debug, Clone)]
pub struct ModelOption {
    pub selection_key: String,
    pub id: String,
    pub desc: String,
    pub variants: Vec<ResolvedModel>,
}

impl ModelOption {
    pub fn from_variants(variants: Vec<ResolvedModel>) -> Self {
        let first = variants
            .first()
            .expect("ModelOption::from_variants requires at least one variant");
        Self {
            selection_key: format!("{}\t{}", first.provider_name, first.id),
            id: first.id.clone(),
            desc: first.desc.clone(),
            variants,
        }
    }

    pub fn default_variant_index(&self, agent_wire_apis: Option<&[WireApi]>) -> usize {
        let filtered: Vec<(usize, &ResolvedModel)> = self
            .variants
            .iter()
            .enumerate()
            .filter(|(_, v)| agent_wire_apis.is_none_or(|apis| apis.contains(&v.wire_api)))
            .collect();
        filtered
            .into_iter()
            .min_by_key(|(_, variant)| variant.wire_api.priority())
            .map(|(index, _)| index)
            .unwrap_or(0)
    }

    pub fn selected_variant_index(
        &self,
        selected_wire_apis: &BTreeMap<String, usize>,
        agent_wire_apis: Option<&[WireApi]>,
    ) -> usize {
        selected_wire_apis
            .get(&self.selection_key)
            .copied()
            .filter(|index| *index < self.variants.len())
            .unwrap_or_else(|| self.default_variant_index(agent_wire_apis))
    }

    pub fn selected_variant<'a>(
        &'a self,
        selected_wire_apis: &BTreeMap<String, usize>,
        agent_wire_apis: Option<&[WireApi]>,
    ) -> &'a ResolvedModel {
        &self.variants[self.selected_variant_index(selected_wire_apis, agent_wire_apis)]
    }

    pub fn formatted_row(
        &self,
        selected_wire_apis: &BTreeMap<String, usize>,
        agent_wire_apis: Option<&[WireApi]>,
    ) -> String {
        let selected = self.selected_variant(selected_wire_apis, agent_wire_apis);
        format!(
            "{:<24} {:<11}  {}",
            self.id,
            selected.wire_api.display(),
            self.desc
        )
    }
}

pub fn resolved_model_supports_agent(model: &ResolvedModel, agent_id: &str) -> bool {
    let agent_id = canonical_agent_id(agent_id);
    model
        .visible_agents
        .iter()
        .any(|a| canonical_agent_id(a) == agent_id)
}

// ══════════════════════════════════════════════════
// ChatGPT selection
// ══════════════════════════════════════════════════

pub fn injected_models_for_chatgpt_app(
    all_models: &[ResolvedModel],
    provider_name: &str,
) -> Vec<ResolvedModel> {
    let mut models: Vec<ResolvedModel> = all_models
        .iter()
        .filter(|m| {
            m.provider_name == provider_name
                && resolved_model_supports_agent(m, "ChatGPT.app")
                && m.model_wire_apis.contains(&WireApi::Responses)
        })
        .cloned()
        .collect();
    models.sort_by(|a, b| a.id.cmp(&b.id));
    let mut seen = std::collections::HashSet::new();
    models.retain(|m| seen.insert(m.id.clone()));
    models
}

pub fn build_chatgpt_selection(
    config: &CxConfig,
    all_models: &[ResolvedModel],
    provider_name: &str,
    default_model_id: &str,
) -> Result<Selection> {
    let agent = find_agent(config, "ChatGPT.app").context("配置中缺少 `ChatGPT.app` agent")?;
    let provider = providers_for_agent(config, "ChatGPT.app")
        .into_iter()
        .find(|p| p.name == provider_name)
        .with_context(|| format!("`ChatGPT.app` 下未找到 provider `{provider_name}`"))?;
    let mut injected = injected_models_for_chatgpt_app(all_models, &provider.name);
    if injected.is_empty() {
        bail!(
            "Provider `{provider_name}` 下没有支持 Responses wire api 的模型，无法注入 ChatGPT.app"
        );
    }
    let index = injected
        .iter()
        .position(|m| m.id == default_model_id)
        .with_context(|| {
            format!("Provider `{provider_name}` 的注入目录中未找到模型 `{default_model_id}`")
        })?;
    let default_model = injected.remove(index);
    injected.insert(0, default_model.clone());
    Ok(Selection {
        agent_id: agent.id,
        agent_binary: agent.binary,
        agent_args: agent.args,
        agent_env: agent.env,
        selected_wire_api: WireApi::Responses,
        provider,
        model: Some(default_model),
        injected_models: injected,
    })
}

pub fn resolve_chatgpt_app_apikey(provider: &ResolvedProvider) -> Result<String> {
    let Some(source) = provider.apikey_source.as_deref() else {
        bail!(
            "Provider `{}` 需要 API Key 但未配置 apikey_source",
            provider.name
        );
    };
    let apikey = resolve_apikey(source)
        .with_context(|| format!("解析 Provider `{}` 的 API Key 失败", provider.name))?;
    if apikey.is_empty() {
        bail!("ChatGPT.app 需要 API Key，但未提供");
    }
    Ok(apikey)
}

/// 非交互启动 ChatGPT.app。
pub fn launch_chatgpt_app(provider_name: &str, default_model_id: &str) -> Result<()> {
    let config = load_config()?;
    let chatgpt_settings = config.chatgpt_app.clone().unwrap_or_default();
    let all_models = build_all_models(&config);
    // Probe cache not available in library crate; models use their default wire_api.
    let selection = build_chatgpt_selection(&config, &all_models, provider_name, default_model_id)?;
    let apikey = resolve_chatgpt_app_apikey(&selection.provider)?;
    chatgpt_app::launch_with_injection(&selection, &apikey, &[], &chatgpt_settings)
}

/// ChatGPT.app 注入使用的 CODEX_HOME 目录。
pub fn chatgpt_codex_home() -> Result<PathBuf> {
    Ok(cx_state_dir()?.join(".codex"))
}

/// 加载 ChatGPT.app 注入设置。
pub fn chatgpt_app_settings() -> Result<ChatGptAppSettings> {
    Ok(load_config()?.chatgpt_app.unwrap_or_default())
}

pub fn normalize_chatgpt_app_settings(mut settings: ChatGptAppSettings) -> ChatGptAppSettings {
    if settings.supports_websockets == Some(false) {
        settings.supports_websockets = None;
    }
    if let Some(nickname) = settings.nickname.take() {
        let trimmed = nickname.trim();
        if trimmed.is_empty() {
            settings.nickname = None;
        } else {
            settings.nickname = Some(trimmed.to_string());
        }
    }
    settings
}

/// ChatGPT.app 注入时 provider 的展示名。
pub fn chatgpt_provider_display_name<'a>(
    provider_name: &'a str,
    chatgpt_settings: &'a ChatGptAppSettings,
) -> &'a str {
    chatgpt_settings
        .nickname
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .unwrap_or(provider_name)
}

/// 保存 ChatGPT.app 注入设置。
pub fn save_chatgpt_app_settings(settings: &ChatGptAppSettings) -> Result<()> {
    let (mut config, path) = load_config_for_add()?;
    let normalized = normalize_chatgpt_app_settings(settings.clone());
    config.chatgpt_app = if normalized == ChatGptAppSettings::default() {
        None
    } else {
        Some(normalized)
    };
    save_config(&path, &config)
}

/// ChatGPT.app 可注入模型目录。
pub type ChatGptInjectableCatalog = Vec<(String, Result<Vec<String>, String>)>;

/// ChatGPT.app 可注入模型目录（Settings 只读展示）。
pub fn chatgpt_injectable_catalog() -> Result<ChatGptInjectableCatalog> {
    let config = load_config()?;
    let mut catalog = Vec::new();
    for provider in &config.providers {
        if !provider_supports_agent(&config, provider, "ChatGPT.app") {
            continue;
        }
        if !provider.has_endpoints() {
            continue;
        }
        let entry = match resolved_models_for_provider(&config, provider) {
            Ok(resolved) => Ok(injected_models_for_chatgpt_app(&resolved, &provider.name)
                .into_iter()
                .map(|m| m.id)
                .collect()),
            Err(e) => Err(format!("{e:#}")),
        };
        catalog.push((provider.name.clone(), entry));
    }
    Ok(catalog)
}

// ══════════════════════════════════════════════════
// VS Code selection
// ══════════════════════════════════════════════════

/// VS Code Claude Code Extension 可注入模型目录。
pub type VsCodeClaudeCatalog = Vec<(String, Result<Vec<String>, String>)>;

/// VS Code Claude Code Extension 可注入模型目录。
pub fn vscode_claude_injectable_catalog() -> Result<VsCodeClaudeCatalog> {
    let config = load_config()?;
    let mut catalog = Vec::new();
    for provider in &config.providers {
        if !provider_supports_agent(&config, provider, "VS Code") {
            continue;
        }
        if !provider.has_endpoints() {
            continue;
        }
        let entry = match resolved_models_for_provider(&config, provider) {
            Ok(resolved) => Ok(injected_models_for_vscode_claude(&resolved, &provider.name)
                .into_iter()
                .map(|m| m.id)
                .collect()),
            Err(e) => Err(format!("{e:#}")),
        };
        catalog.push((provider.name.clone(), entry));
    }
    Ok(catalog)
}

pub fn injected_models_for_vscode_claude(
    all_models: &[ResolvedModel],
    provider_name: &str,
) -> Vec<ResolvedModel> {
    let mut models: Vec<ResolvedModel> = all_models
        .iter()
        .filter(|m| {
            m.provider_name == provider_name
                && resolved_model_supports_agent(m, "VS Code")
                && m.model_wire_apis.contains(&WireApi::Anthropic)
        })
        .cloned()
        .collect();
    models.sort_by(|a, b| a.id.cmp(&b.id));
    let mut seen = std::collections::HashSet::new();
    models.retain(|m| seen.insert(m.id.clone()));
    models
}

// ══════════════════════════════════════════════════
// Reserved env keys
// ══════════════════════════════════════════════════

pub fn base_reserved_env_keys() -> Vec<String> {
    vec![
        "CODEX_HOME".to_string(),
        "CX_MODEL".to_string(),
        "CX_WARP_SESSION_ID".to_string(),
    ]
}

#[allow(unused)]
pub fn chatgpt_reserved_env_keys(env_key: &str) -> Vec<String> {
    let mut keys = base_reserved_env_keys();
    keys.push(env_key.to_string());
    keys
}

pub fn all_reserved_env_keys(config: &CxConfig) -> Vec<String> {
    let mut keys = base_reserved_env_keys();
    for provider in &config.providers {
        keys.push(env_key_for_apikey_source(provider.apikey_source.as_deref()));
    }
    keys
}

/// 校验 ChatGPT.app 自定义环境变量键。
pub fn validate_chatgpt_custom_env(env: &BTreeMap<String, String>) -> Result<()> {
    if env.is_empty() {
        return Ok(());
    }
    let config = load_config()?;
    let reserved = all_reserved_env_keys(&config);
    let offenders: Vec<&str> = env
        .keys()
        .filter(|k| reserved.iter().any(|r| r == *k))
        .map(|s| s.as_str())
        .collect();
    if offenders.is_empty() {
        Ok(())
    } else {
        bail!("自定义环境变量与 cx 保留键冲突: {}", offenders.join(", "))
    }
}

// ══════════════════════════════════════════════════
// VS Code launch selection
// ══════════════════════════════════════════════════

#[allow(unused)]
pub fn build_vscode_selection(
    config: &CxConfig,
    all_models: &[ResolvedModel],
    provider_name: &str,
    model_id: &str,
) -> Result<Selection> {
    let agent = find_agent(config, "VS Code").context("配置中缺少 `VS Code` agent")?;
    let provider = providers_for_agent(config, "VS Code")
        .into_iter()
        .find(|p| p.name == provider_name)
        .with_context(|| format!("`VS Code` 下未找到 provider `{provider_name}`"))?;
    let model = all_models
        .iter()
        .find(|m| {
            m.provider_name == provider_name
                && m.id == model_id
                && resolved_model_supports_agent(m, "VS Code")
                && m.model_wire_apis.contains(&WireApi::Anthropic)
        })
        .with_context(|| {
            format!("Provider `{provider_name}` 下未找到 VS Code 可用模型 `{model_id}`")
        })?
        .clone();
    Ok(Selection {
        agent_id: agent.id,
        agent_binary: agent.binary,
        agent_args: agent.args,
        agent_env: agent.env,
        selected_wire_api: WireApi::Anthropic,
        provider,
        model: Some(model),
        injected_models: Vec::new(),
    })
}

#[allow(unused)]
pub fn resolve_vscode_apikey(provider: &ResolvedProvider) -> Result<String> {
    let Some(source) = provider.apikey_source.as_deref() else {
        bail!(
            "Provider `{}` 需要 API Key 但未配置 apikey_source",
            provider.name
        );
    };
    let apikey = resolve_apikey(source)
        .with_context(|| format!("解析 Provider `{}` 的 API Key 失败", provider.name))?;
    if apikey.is_empty() {
        bail!("VS Code 注入需要 API Key，但未提供");
    }
    Ok(apikey)
}

/// 加载 VS Code 注入设置。
pub fn vscode_app_settings() -> Result<VsCodeAppSettings> {
    Ok(load_config()?.vscode_app.unwrap_or_default())
}

/// 保存 VS Code 注入设置。
pub fn save_vscode_app_settings(settings: &VsCodeAppSettings) -> Result<()> {
    let (mut config, path) = load_config_for_add()?;
    config.vscode_app = normalize_vscode_app_settings(settings.clone());
    save_config(&path, &config)
}

pub fn normalize_vscode_app_settings(settings: VsCodeAppSettings) -> Option<VsCodeAppSettings> {
    if settings == VsCodeAppSettings::default() {
        None
    } else {
        Some(settings)
    }
}

/// VS Code 的 Claude Code 扩展注入部分。
pub struct VsCodeClaudePart {
    pub selection: Selection,
    pub apikey: String,
}

/// VS Code 的 Codex 扩展注入部分。
pub struct VsCodeCodexPart {
    pub prepared: ChatGptAppPrepared,
    pub apikey: String,
    pub provider_name: String,
    pub model_id: String,
    pub api_model_id: String,
    pub model_env: BTreeMap<String, String>,
}

/// 非交互启动 VS Code。
pub fn launch_vscode_app_from_settings(folder: Option<&Path>) -> Result<()> {
    let config = load_config()?;
    let all_models = build_all_models(&config);
    // Probe cache not available in library crate; models use their default wire_api.
    let settings = config.vscode_app.clone().unwrap_or_default();
    let chatgpt_settings = config.chatgpt_app.clone().unwrap_or_default();
    let claude = resolve_vscode_claude_part(&config, &all_models, &settings.claude_code)?;
    let codex =
        resolve_vscode_codex_part(&config, &all_models, &settings.codex, &chatgpt_settings)?;
    vscode_app::launch(claude.as_ref(), codex.as_ref(), folder)
}

pub fn resolve_vscode_claude_part(
    config: &CxConfig,
    all_models: &[ResolvedModel],
    block: &VsCodeExtensionBlock,
) -> Result<Option<VsCodeClaudePart>> {
    if block.disabled {
        return Ok(None);
    }
    let candidates: Vec<(ResolvedProvider, Vec<ResolvedModel>)> =
        providers_for_agent(config, "VS Code")
            .into_iter()
            .filter(|p| p.has_endpoints)
            .filter_map(|p| {
                let models = injected_models_for_vscode_claude(all_models, &p.name);
                (!models.is_empty()).then_some((p, models))
            })
            .collect();
    let Some((provider, models)) =
        pick_vscode_provider(&candidates, block.provider.as_deref(), "Claude Code")
    else {
        return Ok(None);
    };
    let model_id = models[0].id.clone();
    let selection = build_vscode_selection(config, all_models, &provider.name, &model_id)?;
    let apikey = resolve_vscode_apikey(&selection.provider)?;
    Ok(Some(VsCodeClaudePart { selection, apikey }))
}

pub fn resolve_vscode_codex_part(
    config: &CxConfig,
    all_models: &[ResolvedModel],
    block: &VsCodeExtensionBlock,
    chatgpt_settings: &ChatGptAppSettings,
) -> Result<Option<VsCodeCodexPart>> {
    if block.disabled {
        return Ok(None);
    }
    let candidates: Vec<(ResolvedProvider, Vec<ResolvedModel>)> =
        providers_for_agent(config, "ChatGPT.app")
            .into_iter()
            .filter(|p| p.has_endpoints)
            .filter_map(|p| {
                let models = injected_models_for_chatgpt_app(all_models, &p.name);
                (!models.is_empty()).then_some((p, models))
            })
            .collect();
    let Some((provider, injected)) =
        pick_vscode_provider(&candidates, block.provider.as_deref(), "Codex")
    else {
        return Ok(None);
    };
    let default_model = injected[0].clone();
    let prepared = prepare_chatgpt_launch_home_for_app(
        &default_model,
        provider,
        WireApi::Responses,
        injected,
        chatgpt_settings,
        &provider.name,
    )?;
    let apikey = resolve_chatgpt_app_apikey(provider)?;
    Ok(Some(VsCodeCodexPart {
        api_model_id: default_model.api_model_id(),
        provider_name: provider.name.clone(),
        model_id: default_model.id.clone(),
        model_env: default_model.env.clone(),
        prepared,
        apikey,
    }))
}

pub fn pick_vscode_provider<'a>(
    candidates: &'a [(ResolvedProvider, Vec<ResolvedModel>)],
    configured: Option<&str>,
    block_label: &str,
) -> Option<&'a (ResolvedProvider, Vec<ResolvedModel>)> {
    if candidates.is_empty() {
        return None;
    }
    match configured {
        Some(name) => match candidates.iter().find(|(p, _)| p.name == name) {
            Some(found) => Some(found),
            None => {
                eprintln!(
                    "[cx] 警告: VS Code {block_label} 块配置的 provider `{name}` 不可用，回落第一个兼容 provider"
                );
                Some(&candidates[0])
            }
        },
        None => Some(&candidates[0]),
    }
}

/// VS Code 是否已安装。
pub fn vscode_app_installed() -> bool {
    vscode_app::is_installed()
}

// ══════════════════════════════════════════════════
// Launch spec
// ══════════════════════════════════════════════════

#[derive(Debug)]
pub struct LaunchSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub summary: String,
    pub detach: bool,
    pub env_remove: Vec<String>,
    pub agent_id: String,
    pub provider_name: String,
    pub model_id: Option<String>,
    pub pty: bool,
    pub socket: Option<String>,
    pub cwd: Option<PathBuf>,
}

/// Resolve an explicit (provider, model) launch selection to the concrete
/// endpoint variant plus the wire api to launch with.
#[allow(unused)]
pub fn resolve_launch_model(
    all_models: &[ResolvedModel],
    provider_name: &str,
    agent: &ResolvedAgent,
    model_id: &str,
    wire: Option<WireApi>,
) -> Result<(ResolvedModel, WireApi)> {
    if let Some(w) = wire {
        anyhow::ensure!(
            agent.supports_wire_api(w),
            "agent `{}` 不支持 wire `{}`",
            agent.id,
            w.display()
        );
    }
    let model = all_models
        .iter()
        .find(|m| {
            m.provider_name == provider_name
                && resolved_model_supports_agent(m, &agent.id)
                && m.id == model_id
                && wire.map(|w| m.wire_api == w).unwrap_or(true)
        })
        .with_context(|| {
            format!(
                "provider `{}` 下未找到支持 `{}` 的 model `{}`{}",
                provider_name,
                agent.id,
                model_id,
                wire.map(|w| format!("（wire `{}`）", w.display()))
                    .unwrap_or_default()
            )
        })?
        .clone();
    let selected = wire
        .or_else(|| {
            model
                .model_wire_apis
                .iter()
                .find(|w| agent.supported_wire_apis.contains(w))
                .copied()
        })
        .unwrap_or(model.wire_api);
    Ok((model, selected))
}

pub fn build_launch_spec(
    selection: &Selection,
    passthrough_args: &[String],
    pty: bool,
    socket: Option<String>,
    cwd: Option<PathBuf>,
) -> Result<LaunchSpec> {
    let program = resolve_binary(&selection.agent_binary)?;
    let mut args = Vec::new();
    args.extend(selection.agent_args.iter().cloned());
    let mut env = BTreeMap::new();

    let agent_id = &selection.agent_id;
    let provider = &selection.provider;
    let mut env_remove = Vec::new();

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
        let model = selection.model.as_ref().context(format!(
            "{} 选择了 {}，但没有选中模型",
            agent_id, provider.name
        ))?;

        let (api_model_id, ctx_hint) = parse_model_context_suffix(&model.id);
        env.insert("CX_MODEL".into(), api_model_id.to_string());

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
                env.insert("COPILOT_MODEL".into(), api_model_id.to_string());
                if let Some(n) = ctx_hint {
                    env.insert("COPILOT_PROVIDER_MAX_PROMPT_TOKENS".into(), n.to_string());
                }
                configure_copilot_auth(&mut env, model.copilot_auth, apikey);
                match model.wire_api {
                    WireApi::Anthropic => {
                        env.insert("COPILOT_PROVIDER_TYPE".into(), "anthropic".into());
                    }
                    WireApi::Responses | WireApi::Completions => {
                        env.insert("COPILOT_PROVIDER_TYPE".into(), "openai".into());
                        env.insert(
                            "COPILOT_PROVIDER_WIRE_API".into(),
                            wire_api_launch_value(model.wire_api)?.to_string(),
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
                env.insert("ANTHROPIC_MODEL".into(), api_model_id.to_string());
                if let Some(tokens) = ctx_hint {
                    env.insert("CLAUDE_CODE_MAX_CONTEXT_TOKENS".into(), tokens.to_string());
                }
                args.push("--model".into());
                args.push(api_model_id.to_string());
                args.extend(passthrough_args.iter().cloned());
            }
            "codex" => {
                prepare_codex_launch_home(
                    model,
                    provider,
                    apikey,
                    &mut env,
                    selection.selected_wire_api,
                )?;
                args.extend(passthrough_args.iter().cloned());
            }
            "ChatGPT.app" => {
                bail!("ChatGPT.app 应由注入路径启动，不应进入 build_launch_spec");
            }
            "VS Code" => {
                bail!("VS Code 应由注入路径启动，不应进入 build_launch_spec");
            }
            _ => {
                args.extend(passthrough_args.iter().cloned());
            }
        }
    }

    env.extend(
        selection
            .agent_env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone())),
    );

    if let Some(ref model) = selection.model {
        env.extend(model.env.iter().map(|(k, v)| (k.clone(), v.clone())));
    } else {
        env.extend(provider.env.iter().map(|(k, v)| (k.clone(), v.clone())));
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
        agent_id: agent_id.clone(),
        provider_name: provider.name.clone(),
        model_id: selection.model.as_ref().map(|m| m.id.clone()),
        pty,
        socket,
        cwd,
    })
}

pub fn configure_copilot_auth(
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

pub fn resolve_binary(name: &str) -> Result<PathBuf> {
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

// ══════════════════════════════════════════════════
// Secret prompting — 交互式补齐缺失的 API Key
// ══════════════════════════════════════════════════

pub fn resolve_apikey_interactive(source: &str) -> Result<String> {
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

// ══════════════════════════════════════════════════
// Extern crate probe — re-export for sub-crates that need it
// ══════════════════════════════════════════════════

// The probe module is defined in the `cx` crate (CLI half). Re-export the
// probe trait so that vscode_app and chatgpt_app can reference it.
// In the library crate, we use a blocking list_models call instead.
mod probe {
    /// Runtime handle for the tokio executor, used by the cx crate's probe module.
    /// In the library crate, we provide a simple blocking fallback.
    pub(crate) fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Runtime::new().expect("failed to create tokio runtime for probe")
    }

    pub(crate) mod db {
        /// Stub: the probe database is only available in the cx crate.
        /// In the library crate, we skip probe cache.
        pub fn get_available_wire_api(
            _conn: &rusqlite::Connection,
            _provider: &str,
            _model: &str,
        ) -> Result<Option<super::super::WireApi>, rusqlite::Error> {
            Ok(None)
        }
    }
}
