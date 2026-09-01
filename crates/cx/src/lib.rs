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
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::{self, Write as IoWrite};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::process::{CommandExt, ExitStatusExt};
#[cfg(windows)]
use std::os::windows::fs::{symlink_dir, symlink_file};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use url::Url;

use manox_providers::{
    AgentConfig, ApiKeySourceKind, ChatGptAppSettings, CopilotAuth, CxConfig, EndpointConfig,
    ModelConfig, PROVIDER_CONFIG_FILE_NAME, ProviderConfig, ProviderEndpointSpec,
    ProviderModelConfig, ProviderModels, ResolvedAgent, ResolvedModel, VsCodeAppSettings,
    VsCodeExtensionBlock, WireApi, active_provider_config_path, canonical_agent_id,
    context_window_from_suffix, cx_state_dir, effective_agents_for_model, read_config_file,
    resolve_apikey, resolved_agents,
};

mod chatgpt_app;
mod probe;
mod relay;
mod session;
mod stats;
mod vscode_app;
mod warp;

/// Programmatic agent launch API (builder + live session handle).
pub mod api;
pub use api::{Agent, AgentBuilder, SessionHandle, SessionResult};

/// Out-of-process message injection into a running session (`cx send` core).
pub mod send;
pub use send::{CLEAR_INPUT_BYTE, SendSelector, SendTarget, send};

const SEND_AFTER_HELP: &str = "\
示例:
  cx send \"hi\"                                    注入到最近的 session
  cx send --session claude \"hi\"                   注入到最近的 claude session
  cx send --session <ID> \"hi\"                     按 session id 精确定位
  cx send --session claude --clear-buffer          清空最近 claude session 的输入区
  cx send --session claude --clear-buffer \"新任务\" 清空后覆盖写入并提交

协议: 经 session socket 发送 line-JSON {\"text\":...}，relay 转写为 text + 回车。
--clear-buffer 在 text 前拼接 Ctrl+U(0x15)，实测在 claude code 中可清空输入区。
";

const VERSION: &str = env!("CARGO_PKG_VERSION");
const LAUNCH_HOME_DIR_NAME: &str = "cx-launch-homes";
const LAUNCH_HOME_TTL_SECS: u64 = 60 * 60 * 24;
const ADD_PROVIDER_SENTINEL: &str = "+ 添加 Provider";
const ADD_NEW_PROVIDER_SENTINEL: &str = "+ 新建 Provider";
const ADD_WIRE_API_ACTION: &str = "添加 wire_api";
const ADD_MODEL_ACTION: &str = "添加 model";
const DEFAULT_PROVIDER_CONFIG_YAML: &str = include_str!("../config/providers.default.yaml");

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

// resolved_model_from_config delegates to the shared ResolvedModel::from_config
// in manox-providers — the single source of truth for capability defaults and
// agent/endpoint compatibility resolution.
fn resolved_model_from_config(
    config: &CxConfig,
    provider: &ProviderConfig,
    endpoint: &EndpointConfig,
    model: &ModelConfig,
) -> ResolvedModel {
    ResolvedModel::from_config(config, provider, endpoint, model)
}

/// True if `model` lists `agent_id` (modulo `canonical_agent_id` normalization).
fn resolved_model_supports_agent(model: &ResolvedModel, agent_id: &str) -> bool {
    let agent_id = canonical_agent_id(agent_id);
    model
        .visible_agents
        .iter()
        .any(|a| canonical_agent_id(a) == agent_id)
}

#[derive(Debug, Clone)]
struct ModelOption {
    selection_key: String,
    id: String,
    desc: String,
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
            desc: first.desc.clone(),
            variants,
        }
    }

    fn default_variant_index(&self, agent_wire_apis: Option<&[WireApi]>) -> usize {
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

    fn selected_variant_index(
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

    fn selected_variant<'a>(
        &'a self,
        selected_wire_apis: &BTreeMap<String, usize>,
        agent_wire_apis: Option<&[WireApi]>,
    ) -> &'a ResolvedModel {
        &self.variants[self.selected_variant_index(selected_wire_apis, agent_wire_apis)]
    }

    fn formatted_row(
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

/// Model 列表表头，与 `ModelOption::formatted_row()` 使用相同的列宽格式。
/// 左侧 3 spaces 对齐 `highlight_symbol("✨ ")` 的 3 列宽偏移。
fn model_header_row() -> Line<'static> {
    let hdr_style = Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD);
    let header_text = format!("{:<24} {:<11}", "Model", "wire_api");
    Line::from(vec![
        Span::styled("   ", Style::default()),
        Span::styled(header_text, hdr_style),
    ])
}

/// Normalize a CLI-supplied agent name: case-insensitive, with alias collapsing.
///
/// `CoDex.App` / `codexapp` / `codex_app` / `ChatGPT.App` / `chatgptapp` all resolve
/// to the `ChatGPT.app` agent; the remaining ids (`claude`, `codex`, `copilot`)
/// are lowercased verbatim. This only touches user-facing input — internal config/registry
/// ids are already canonical.
fn canonicalize_agent_name(input: &str) -> String {
    match input.to_lowercase().as_str() {
        "codex_app" | "codexapp" | "codex.app" | "chatgpt_app" | "chatgptapp" | "chatgpt.app" => {
            "ChatGPT.app".into()
        }
        "vscode" | "vs-code" | "vs_code" | "vs code" | "vscode.app" => "VS Code".into(),
        other => other.into(),
    }
}

fn find_agent(config: &CxConfig, agent_id: &str) -> Option<ResolvedAgent> {
    let normalized = canonicalize_agent_name(agent_id);
    let agent_id = canonical_agent_id(&normalized);
    resolved_agents(config)
        .into_iter()
        .find(|agent| agent.id == agent_id)
}

/// Map a `WireApi` to the launch vocabulary used by copilot's `COPILOT_PROVIDER_WIRE_API`.
/// `Unavailable` is an error here (cx probe must refresh) — unlike `WireApi::display`,
/// which is a lossless label.
fn wire_api_launch_value(wire_api: WireApi) -> Result<&'static str> {
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
struct ResolvedProvider {
    name: String,
    has_endpoints: bool,
    apikey_source: Option<String>,
    env: BTreeMap<String, String>,
}

impl ResolvedProvider {
    fn from_config(config: &ProviderConfig) -> Self {
        Self {
            name: config.name.clone(),
            has_endpoints: config.has_endpoints(),
            apikey_source: config.apikey_source.clone(),
            env: config.env.clone(),
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
    selected_wire_api: WireApi,
    provider: ResolvedProvider,
    model: Option<ResolvedModel>,
    /// 仅 ChatGPT.app 等注入型 agent 使用：注入给桌面端的完整模型列表。
    /// 首个元素为默认模型（写入 config.toml 的 `model =`，并在注入脚本里标记 `isDefault`）。
    /// 非 ChatGPT.app agent 始终为空 Vec。
    injected_models: Vec<ResolvedModel>,
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

fn compatible_agents_for_wire_api(config: &CxConfig, wire_api: WireApi) -> Vec<String> {
    available_agents_for_add(config)
        .into_iter()
        .filter(|agent| agent.supports_wire_api(wire_api))
        .map(|agent| agent.id)
        .collect()
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

/// 若 `dst` 已存在，在重建符号链接前先移除，保证 `materialize_passthrough_dir` 幂等。
///
/// 需处理的两种残留：
/// - 上次创建的**符号链接**（持久目录二次启动）；
/// - ChatGPT.app 用 atomic-rename 写状态文件时，把我们的符号链接**替换成的普通文件**
///   （`rename(tmp, target)` 会覆盖符号链接，产生真实文件，如 `.codex-global-state.json.bak`、
///   `logs_2.sqlite-wal`）。此时 `real` 目录里有真实数据源，重新符号链接到 `real` 即可，不丢数据。
///
/// 仅移除符号链接与普通文件；**真实目录予以保留**，避免误删 Codex 写入的状态目录（如 `sessions/`）。
fn remove_existing_entry(dst: &Path) {
    let Ok(meta) = fs::symlink_metadata(dst) else {
        return; // 不存在，无需处理
    };
    let ft = meta.file_type();
    if ft.is_dir() && !ft.is_symlink() {
        // 真实目录（非指向目录的符号链接）：保留，交由调用方决定是否覆盖。
        return;
    }
    // 符号链接（含指向目录的）或普通文件：移除后由 symlink_path 重建。
    #[cfg(unix)]
    {
        let _ = fs::remove_file(dst); // remove_file 对符号链接（含指向目录的）与普通文件均有效
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
fn symlink_path(src: &Path, dst: &Path) -> Result<()> {
    remove_existing_entry(dst);
    // 若 dst 仍存在，说明是一个保留的真实目录（见 remove_existing_entry）：
    // 不覆盖、不报错，跳过本次符号链接（如 Codex 在 CODEX_HOME 自建的 sessions/）。
    if dst.exists() {
        return Ok(());
    }
    std::os::unix::fs::symlink(src, dst)
        .with_context(|| format!("创建符号链接失败: {} -> {}", dst.display(), src.display()))
}

#[cfg(windows)]
fn symlink_path(src: &Path, dst: &Path) -> Result<()> {
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

/// 把 provider 名称转成 ASCII-safe 的 config.toml provider key。
/// 非字母数字/`-`/`_` 字符被丢弃（如中文「百炼」→ ""）；结果为空时回落到 "custom"。
fn provider_config_key(provider_name: &str) -> String {
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

/// 从 apikey_source 推导 Codex config.toml 的 `env_key`（Codex 运行时读取 API Key 的环境变量名）。
/// `keychain:VAR` / `env:VAR` → `VAR`；其余（`literal:` / `$(shell ...)` / None）回落到 `CX_PROVIDER_KEY`。
fn env_key_for_apikey_source(source: Option<&str>) -> String {
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

/// 从既有 config.toml 文本中提取顶层 `model_reasoning_effort` 的值（去引号）。
/// 用于在重写 config 时保留用户偏好，而非硬编码覆盖。
/// 仅匹配任何 `[section]` 之前的顶层键，避免误取 `[model_providers.*]` 等段内同名字段。
fn extract_reasoning_effort(existing: Option<&str>) -> Option<String> {
    let existing = existing?;
    for line in existing.lines() {
        let trimmed = line.trim();
        // 进入任意 section 后，顶层键已结束，停止扫描。
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

/// 把 `WireApi` 映射成 codex 家族 config.toml 的 `wire_api` 词汇。
/// codex（ChatGPT.app 引擎）读取的规范值为 `responses` / `chat_completions` / `anthropic_messages`，
/// 与 cx 内部 `WireApi::launch_value()`（供 copilot 的 `COPILOT_PROVIDER_WIRE_API` 使用，
/// 值为 `responses` / `completions` / `anthropic`）不同，故单独提供。
fn codex_wire_api_str(wire_api: WireApi) -> Result<&'static str> {
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
fn merge_codex_config(
    existing: Option<&str>,
    model: &ResolvedModel,
    workspace_root: &Path,
    wire_api: WireApi,
    provider_key: &str,
    provider_name: &str,
    env_key: &str,
    // 发给 provider 的 model id（已剥除 cx 内部上下文后缀）。
    api_model_id: &str,
    // 模型上下文窗口 token 数（来自上下文后缀，如 `[1m]`/`[200k]`），写入 codex config.toml 的
    // `model_context_window`。None 则不写、保留用户既有值。
    context_window: Option<i64>,
    // 本地模型目录（ModelsResponse JSON）路径，写入 config.toml 的 `model_catalog_json`。
    // 让引擎「认识」自定义模型、跳过 fallback 元数据对上下文窗口的钳制。None 则不写。
    model_catalog_json: Option<&str>,
) -> Result<String> {
    let project_section = format!(
        "[projects.{}]",
        toml_basic_string(&workspace_root.to_string_lossy())
    );
    // 保留用户已设置的 reasoning effort（若无则回落到 "high"，与 POC 一致）。
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

            // 所有 [model_providers.*] section（含历史硬编码的 dashscope）都由本次重新生成，保留时整体跳过
            if trimmed.starts_with("[model_providers.") || trimmed == project_section {
                skipping_section = true;
                continue;
            }

            if !trimmed.starts_with('[')
                && (trimmed.starts_with("model =")
                    || trimmed.starts_with("model_provider =")
                    || trimmed.starts_with("model_reasoning_effort =")
                    // cx 本次要重写 model_context_window 时，剥离用户旧值以免冲突。
                    || (context_window.is_some()
                        && trimmed.starts_with("model_context_window ="))
                    // cx 本次要重写 model_catalog_json 时，剥离用户旧值以免冲突。
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

/// 在 merge_codex_config 渲染结果中注入 `supports_websockets = <bool>`。
/// 插入点是首个 `wire_api = ...` 行之后——merge_codex_config 会整体丢弃用户
/// `[model_providers.*]` 段并只重新生成本次注入的一个，因此首个匹配即注入段；
/// 保留的用户内容排在其后，不受影响。找不到插入点时原样返回（防御）。
fn inject_supports_websockets(config: &str, supports: bool) -> String {
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

fn prepare_codex_launch_home(
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
    // 剥除上下文后缀：provider 接收的是 base id（如 glm-5.2），
    // 1m 上下文信息写入 codex 的 model_context_window。
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
/// 使用固定目录 ~/.manox/.codex/，Symlink 真实 ~/.codex/ 内容（config.toml 除外），
/// 写入我们注入的 config.toml（动态 provider key / env_key）。Codex Desktop 读 CODEX_HOME 指向此目录。
///
/// 返回 `ChatGptAppPrepared`：codex_home 供调用方在启动子进程时设 `CODEX_HOME` 环境变量，
/// env_key 是 config.toml 里 Codex 运行时读取 API Key 的环境变量名，
/// reasoning_effort 是解析出的（或默认 "high"）推理强度，供注入脚本与下拉默认值保持一致。
fn prepare_chatgpt_launch_home_for_app(
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

    // 创建固定目录
    if !codex_dir.exists() {
        std::fs::create_dir_all(&codex_dir)?;
    }

    // Symlink 真实 .codex 内容（auth.json 等），config.toml 除外
    materialize_passthrough_dir(&real_codex_dir, &codex_dir, &["config.toml"])?;

    let provider_key = provider_config_key(&provider.name);
    let env_key = env_key_for_apikey_source(provider.apikey_source.as_deref());
    // 读取真实 config.toml（如有）做保留
    let existing_config = fs::read_to_string(real_codex_dir.join("config.toml")).ok();
    let reasoning_effort =
        extract_reasoning_effort(existing_config.as_deref()).unwrap_or_else(|| "high".to_string());
    let (api_model_id, context_window) = parse_model_context_suffix(&model.id);
    // 生成本地模型目录（收录带上下文后缀的模型），让引擎跳过 fallback 对上下文窗口的钳制。
    // 无后缀模型时目录为空（引擎拒绝空目录），此时不写 model_catalog_json，保持现状。
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
        // 展示名可被昵称覆盖（ChatGPT.app 注入路径），provider key 不变。
        provider_display_name,
        &env_key,
        api_model_id,
        context_window,
        model_catalog_json.as_deref(),
    )?;
    // supports_websockets（Settings → 外部工具开放项）：merge_codex_config 重新生成
    // 且唯一持有 `[model_providers.*]` 段，在其后注入该行即可。未配置按 false——
    // 自定义 provider 普遍不支持 WS 流式，引擎默认 true 会导致请求失败。
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

/// `prepare_chatgpt_launch_home_for_app` 的产物，供 chatgpt_app 启动编排使用。
struct ChatGptAppPrepared {
    codex_home: PathBuf,
    env_key: String,
    reasoning_effort: String,
    /// Settings 配置的自定义环境变量（保留键在注入时过滤）。
    custom_env: BTreeMap<String, String>,
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
            chatgpt_app: None,
            vscode_app: None,
            subagents: Default::default(),
        }
    };
    Ok((config, path))
}

fn save_config(path: &Path, config: &CxConfig) -> Result<()> {
    let yaml = serde_yaml::to_string(config).context("序列化配置失败")?;
    write_string_atomic(path, &yaml)
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

/// 解析单个 provider 下的全部模型（在线拉取 + endpoint 归一）。
/// 网络/解析失败以 Err 返回，由调用方决定可见性：启动路径回落跳过，
/// Settings 目录则以「加载失败」条目呈现，避免把网络故障误归因于配置。
fn resolved_models_for_provider(
    config: &CxConfig,
    provider: &ProviderConfig,
) -> Result<Vec<ResolvedModel>> {
    let fetched = probe::runtime().block_on(provider.list_models())?;
    let mut models = Vec::new();
    for endpoint in provider.normalized_endpoints_resolved(&fetched) {
        for model in &endpoint.models {
            models.push(resolved_model_from_config(
                config, provider, &endpoint, model,
            ));
        }
    }
    Ok(models)
}

fn provider_supports_agent(config: &CxConfig, provider: &ProviderConfig, agent_id: &str) -> bool {
    let agent_id = canonical_agent_id(agent_id);
    if !provider.has_endpoints() {
        return true;
    }
    // Remote-model providers: assume compatible until models are fetched at
    // selection time; the actual model list determines compatibility then.
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

fn apply_probe_cache(models: &mut [ResolvedModel]) {
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
                // 没有可用的 wire_api，标记为 Unavailable
                model.wire_api = WireApi::Unavailable;
            }
            Err(_) => {
                // 查询失败，保持原有 wire_api
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
        env: BTreeMap::new(),
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

/// 收集某 provider 下所有支持 ChatGPT.app（即 wire 含 Responses）的模型，作为注入桌面端的完整列表。
///
/// 同一 model id 仅保留一条，按 model id 升序排序。首个元素即默认模型。
fn injected_models_for_chatgpt_app(
    all_models: &[ResolvedModel],
    provider_name: &str,
) -> Vec<ResolvedModel> {
    let mut models: Vec<ResolvedModel> = all_models
        .iter()
        .filter(|m| {
            m.provider_name == provider_name
                && resolved_model_supports_agent(m, "ChatGPT.app")
                // 显式确认模型支持 Responses wire api。supports_agent 已隐含这点
                // （ChatGPT.app agent 仅兼容 Responses endpoint），但这里再过滤一次，
                // 防止配置层不变量将来变动时把非 Responses 模型注入、生成错误的 wire_api。
                && m.model_wire_apis.contains(&WireApi::Responses)
        })
        .cloned()
        .collect();
    models.sort_by(|a, b| a.id.cmp(&b.id));
    let mut seen = std::collections::HashSet::new();
    models.retain(|m| seen.insert(m.id.clone()));
    models
}

/// 构造 ChatGPT.app 启动所需的 `Selection`：provider 下全部 Responses 模型作为注入目录，
/// 调用方选中的默认模型置顶（`injected_models[0]` = 默认模型的既有约定，
/// 写入 config.toml 的 `model =` 并在注入脚本里标记 `isDefault`）。
fn build_chatgpt_selection(
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

/// ChatGPT.app 的 API Key 非交互解析（GUI 嵌入路径）：无 stdin 可补齐，缺失即报错。
fn resolve_chatgpt_app_apikey(provider: &ResolvedProvider) -> Result<String> {
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

/// ChatGPT.app 的 API Key 交互解析（CLI 路径）：Keychain 缺失时提示输入并回写。
fn resolve_chatgpt_app_apikey_interactive(provider: &ResolvedProvider) -> Result<String> {
    let Some(source) = provider.apikey_source.as_deref() else {
        bail!(
            "Provider `{}` 需要 API Key 但未配置 apikey_source",
            provider.name
        );
    };
    let apikey = resolve_apikey_interactive(source)?;
    if apikey.is_empty() {
        bail!("ChatGPT.app 需要 API Key，但未提供");
    }
    Ok(apikey)
}

/// 非交互启动 ChatGPT.app：显式指定 provider 与默认模型，无 TUI。供 GUI 嵌入方
/// （manox 系统菜单）调用——选中模型作为默认模型，同时注入该 provider 下全部
/// Responses 模型目录。阻塞调用（配置加载、模型列表构建、CDP 注入最长约 20s），
/// 调用方应在后台线程执行。
pub fn launch_chatgpt_app(provider_name: &str, default_model_id: &str) -> Result<()> {
    let config = load_config()?;
    let chatgpt_settings = config.chatgpt_app.clone().unwrap_or_default();
    let mut all_models = build_all_models(&config);
    apply_probe_cache(&mut all_models);
    let selection = build_chatgpt_selection(&config, &all_models, provider_name, default_model_id)?;
    let apikey = resolve_chatgpt_app_apikey(&selection.provider)?;
    chatgpt_app::launch_with_injection(&selection, &apikey, &[], &chatgpt_settings)
}

/// ChatGPT.app 注入使用的 CODEX_HOME 目录（Settings 只读展示）。
pub fn chatgpt_codex_home() -> Result<PathBuf> {
    Ok(cx_state_dir()?.join(".codex"))
}

/// 加载 ChatGPT.app 注入设置（`chatgpt_app:` 段缺失时返回默认空设置）。
pub fn chatgpt_app_settings() -> Result<ChatGptAppSettings> {
    Ok(load_config()?.chatgpt_app.unwrap_or_default())
}

/// `supports_websockets: Some(false)` 归一为 `None`（启动端 `None` 按 false
/// 处理，语义等价），`nickname` 空白归一为 `None`，使净零操作与
/// `default()` 相等、整段可省略。纯函数，便于单测。
fn normalize_chatgpt_app_settings(mut settings: ChatGptAppSettings) -> ChatGptAppSettings {
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

/// ChatGPT.app 注入时 provider 的展示名：昵称（trim 非空）优先，否则
/// provider 本名。CLI 与 GUI 启动路径共用，保证 config.toml 的
/// `[model_providers.*] name` 与启动摘要一致。
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

/// 保存 ChatGPT.app 注入设置（保留配置文件的 providers/agents 等其余段）。
pub fn save_chatgpt_app_settings(settings: &ChatGptAppSettings) -> Result<()> {
    let (mut config, path) = load_config_for_add()?;
    // 归一后全默认时整段省略，避免净零操作残留 `chatgpt_app:` 段。
    let normalized = normalize_chatgpt_app_settings(settings.clone());
    config.chatgpt_app = if normalized == ChatGptAppSettings::default() {
        None
    } else {
        Some(normalized)
    };
    save_config(&path, &config)
}

/// ChatGPT.app 可注入模型目录：provider 名 → 可注入 model id 列表，
/// 或该 provider 的拉取错误（面板以「加载失败」呈现，不静默省略）。
pub type ChatGptInjectableCatalog = Vec<(String, Result<Vec<String>, String>)>;

/// ChatGPT.app 可注入模型目录（Settings 只读展示）：每个支持 ChatGPT.app
/// 注入的 provider → 其可注入模型（Responses 能力 + ChatGPT.app 可见）。
/// per-provider 拉取失败以 Err 条目返回（面板显示「加载失败」），不静默省略。
pub fn chatgpt_injectable_catalog() -> Result<ChatGptInjectableCatalog> {
    let config = load_config()?;
    let mut catalog = Vec::new();
    for provider in &config.providers {
        if !provider_supports_agent(&config, provider, "ChatGPT.app") {
            continue;
        }
        // 与启动候选同一谓词：无 endpoints 的 provider 无注入目标，不列入目录。
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

/// VS Code Claude Code Extension 可注入模型目录（Settings 只读展示）：每个
/// 支持 VS Code（Anthropic wire）的 provider → 其可见模型。per-provider
/// 拉取失败以 Err 条目返回（面板显示「加载失败」），不静默省略。
pub type VsCodeClaudeCatalog = Vec<(String, Result<Vec<String>, String>)>;

/// VS Code Claude Code Extension 可注入模型目录。
pub fn vscode_claude_injectable_catalog() -> Result<VsCodeClaudeCatalog> {
    let config = load_config()?;
    let mut catalog = Vec::new();
    for provider in &config.providers {
        if !provider_supports_agent(&config, provider, "VS Code") {
            continue;
        }
        // 与启动候选同一谓词：无 endpoints 的 provider 无注入目标，不列入目录。
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

/// 收集某 provider 下所有 VS Code 可见（Anthropic wire）的模型，按 model id
/// 升序排序，同一 id 仅保留一条。首个元素即默认模型（与 ChatGPT.app 目录同约定）。
fn injected_models_for_vscode_claude(
    all_models: &[ResolvedModel],
    provider_name: &str,
) -> Vec<ResolvedModel> {
    let mut models: Vec<ResolvedModel> = all_models
        .iter()
        .filter(|m| {
            m.provider_name == provider_name
                && resolved_model_supports_agent(m, "VS Code")
                // 显式确认 Anthropic wire（supports_agent 已隐含，防配置层不变量漂移，
                // 与 build_vscode_selection 的过滤保持一致）。
                && m.model_wire_apis.contains(&WireApi::Anthropic)
        })
        .cloned()
        .collect();
    models.sort_by(|a, b| a.id.cmp(&b.id));
    let mut seen = std::collections::HashSet::new();
    models.retain(|m| seen.insert(m.id.clone()));
    models
}

/// cx 托管的基础保留环境变量键（与 provider 无关）。
fn base_reserved_env_keys() -> Vec<String> {
    vec![
        "CODEX_HOME".to_string(),
        "CX_MODEL".to_string(),
        "CX_WARP_SESSION_ID".to_string(),
    ]
}

/// cx 托管的保留环境变量键：用户自定义 env 不允许覆盖。
/// 启动注入端使用：基础保留键 + 所选 provider 的 env_key。
pub(crate) fn chatgpt_reserved_env_keys(env_key: &str) -> Vec<String> {
    let mut keys = base_reserved_env_keys();
    keys.push(env_key.to_string());
    keys
}

/// 保存校验端保留键：基础保留键 + 全部 provider 的 env_key。
/// 是启动端过滤的超集（「校验 ⊇ 启动过滤」不变量，见测试）。
fn all_reserved_env_keys(config: &CxConfig) -> Vec<String> {
    let mut keys = base_reserved_env_keys();
    for provider in &config.providers {
        keys.push(env_key_for_apikey_source(provider.apikey_source.as_deref()));
    }
    keys
}

/// 校验 ChatGPT.app 自定义环境变量键：与 cx 保留键
/// （CODEX_HOME / CX_MODEL / CX_WARP_SESSION_ID / 各 provider env_key）冲突即报错。
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

/// 构造 VS Code 启动所需的 `Selection`：选中模型即唯一 BYOK 目标（无模型目录
/// 注入——Claude Code 直接消费 ANTHROPIC_MODEL / ANTHROPIC_BASE_URL env）。
fn build_vscode_selection(
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
                // 显式确认 Anthropic wire（supports_agent 已隐含，防配置层不变量漂移）
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

/// VS Code 的 API Key 非交互解析（GUI 嵌入路径）：无 stdin 可补齐，缺失即报错。
fn resolve_vscode_apikey(provider: &ResolvedProvider) -> Result<String> {
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

/// 加载 VS Code 注入设置（`vscode_app:` 段缺失时返回默认空设置）。
pub fn vscode_app_settings() -> Result<VsCodeAppSettings> {
    Ok(load_config()?.vscode_app.unwrap_or_default())
}

/// 保存 VS Code 注入设置（保留配置文件的 providers/agents 等其余段）。
/// 归一后全默认时整段省略；双「不注入」不等于默认，必须保留。
pub fn save_vscode_app_settings(settings: &VsCodeAppSettings) -> Result<()> {
    let (mut config, path) = load_config_for_add()?;
    config.vscode_app = normalize_vscode_app_settings(settings.clone());
    save_config(&path, &config)
}

fn normalize_vscode_app_settings(settings: VsCodeAppSettings) -> Option<VsCodeAppSettings> {
    if settings == VsCodeAppSettings::default() {
        None
    } else {
        Some(settings)
    }
}

/// VS Code 的 Claude Code 扩展注入部分（ANTHROPIC_* BYOK env）。
pub(crate) struct VsCodeClaudePart {
    pub selection: Selection,
    pub apikey: String,
}

/// VS Code 的 Codex 扩展注入部分（CODEX_HOME + config.toml 官方机制，
/// 复用 ChatGPT.app 的准备逻辑）。
pub(crate) struct VsCodeCodexPart {
    pub prepared: ChatGptAppPrepared,
    pub apikey: String,
    /// 启动摘要展示用。
    pub provider_name: String,
    pub model_id: String,
    /// 发给 provider 的 model id（剥除上下文后缀）；Claude 部分缺席时用作 CX_MODEL。
    pub api_model_id: String,
    /// Provider + Model 级自定义环境变量（ResolvedModel.env 已合并）。
    pub model_env: BTreeMap<String, String>,
}

/// 非交互启动 VS Code：读取持久化 `vscode_app:` 设置，按需注入
/// Claude Code Extension（ANTHROPIC_* env）与 Codex Extension
///（CODEX_HOME + config.toml，复用 ChatGPT.app 注入机制）；两块均不注入时
/// 等效普通打开。供 GUI 嵌入方（manox 系统菜单 / 侧边栏单一入口）与 CLI TUI
/// 调用。`folder` 为 `Some` 时新实例直接打开该目录（侧边栏项目路径）；
/// 系统菜单路径传 `None`。
/// 机制见 `vscode_app` 模块：解析登录 shell env 后以最高优先级叠加注入 env，
/// 带 VSCODE_CLI=1 启动，令扩展宿主 / 各扩展 / 集成终端继承注入值。
/// 阻塞调用（shell 解析至多 10s；VS Code 运行中时含确认与退出等待，至多约
/// 60s+），调用方应在后台线程执行。
pub fn launch_vscode_app_from_settings(folder: Option<&Path>) -> Result<()> {
    let config = load_config()?;
    let mut all_models = build_all_models(&config);
    apply_probe_cache(&mut all_models);
    let settings = config.vscode_app.clone().unwrap_or_default();
    let chatgpt_settings = config.chatgpt_app.clone().unwrap_or_default();
    let claude = resolve_vscode_claude_part(&config, &all_models, &settings.claude_code)?;
    let codex =
        resolve_vscode_codex_part(&config, &all_models, &settings.codex, &chatgpt_settings)?;
    vscode_app::launch(claude.as_ref(), codex.as_ref(), folder)
}

/// 解析 VS Code 的 Claude Code 扩展注入部分。
///
/// `disabled` 返回 `None`；无任何可注入 provider（候选为空）同样返回 `None`
///（等效未配置，按不注入处理）。持久化 provider 不可用时回落第一个兼容
/// provider 并告警。默认模型 = 该 provider 首个 id 序 Anthropic wire 模型。
fn resolve_vscode_claude_part(
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

/// 解析 VS Code 的 Codex 扩展注入部分。
///
/// 候选 / 回落 / 告警语义同 Claude 部分（Responses 目录非空的 provider）。
/// 默认模型 = `injected_models_for_chatgpt_app` 首元素；全量目录写入
/// model-catalog.json。`chatgpt_app:` 段设置（env / supports_websockets）复用。
fn resolve_vscode_codex_part(
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
        // 昵称是 ChatGPT.app 注入专属配置；VS Code Codex 扩展复用同一
        // config.toml 机制时保持 provider 本名。
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

/// 按 vscode_app 块配置挑选 provider：显式指定优先；缺失或已不可用时回落
/// 第一个兼容候选（不可用另告警）。候选为空返回 None。
fn pick_vscode_provider<'a>(
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

/// VS Code 是否已安装（供 manox 菜单禁用决策）。
pub fn vscode_app_installed() -> bool {
    vscode_app::is_installed()
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
    disable_version_flag = true,
    after_help = "\
用 `--` 分隔 cx 自身参数与被启动 agent 的参数：
  cx                       进 TUI 选 agent（直连）
  cx --pty                 进 TUI 选 agent（PTY 中继，支持 `cx send` 注入）
  cx --pty -- claude --dangerously-skip-permissions   跳过 agent 选择，透传该 flag
  cx --pty -- --dangerously-skip-permissions          进 TUI 选 agent，选定后透传该 flag
  cx --pty -S /tmp/cx.sock -- claude                 自定义 IPC socket 路径
  cx -- ChatGPT.App        ChatGPT.app 桌面端（大小写不敏感，等价 `cx -- chatgpt.app`；旧名 `Codex.app` 仍兼容）"
)]
struct Cli {
    /// 经 PTY 中继启动（cx 持 master，终端 IO 透传，并暴露外部 IPC 注入入口）；默认直连。
    #[arg(long)]
    pty: bool,
    /// 自定义 IPC socket 路径（仅与 --pty 同时生效；默认 ~/.manox/sessions/<id>.sock）。
    #[arg(long, short = 'S', value_name = "SOCK PATH", requires = "pty")]
    socket: Option<String>,
    /// Agent 进程的工作目录；默认继承 cx 自身 cwd。绑定外部 CLI session 到项目目录时用。
    #[arg(long, value_name = "DIR")]
    cwd: Option<PathBuf>,
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
    /// 查看各 agent × model 的 token 用量统计（TUI / 图片输出）
    Stats {
        #[arg(long, value_name = "FORMAT")]
        output: Option<String>,
        #[arg(long, value_name = "VIEW")]
        view: Option<String>,
        #[arg(long, value_name = "PERIOD")]
        period: Option<String>,
    },
    /// 向运行中的 cx agent session 注入一条消息（当作用户输入提交）
    #[command(after_help = SEND_AFTER_HELP)]
    Send {
        /// 目标 session：<ID> 精确定位 / latest 最近一个 / claude|codex|copilot 取该 agent 最近一个
        #[arg(long, value_name = "ID|latest|claude|codex|copilot")]
        session: Option<String>,
        /// 注入前先送 Ctrl+U(0x15) 清空输入区；text 为空=仅清空，非空=清空后覆盖写入并提交
        #[arg(long)]
        clear_buffer: bool,
        /// 要注入的文本（与 --clear-buffer 至少传其一）
        text: Option<String>,
    },
    /// 启动 WebUI 服务（浏览器界面）
    Web {
        /// 监听端口（默认随机分配）
        #[arg(long, value_name = "PORT")]
        port: Option<u16>,
        /// 不自动打开浏览器
        #[arg(long)]
        no_open: bool,
    },
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
    Stats {
        output: Option<String>,
        view: Option<String>,
        period: Option<String>,
    },
    Send {
        session: Option<String>,
        clear_buffer: bool,
        text: Option<String>,
    },
    Launch {
        args: Vec<String>,
        pty: bool,
        socket: Option<String>,
        cwd: Option<PathBuf>,
    },
    Web {
        port: Option<u16>,
        no_open: bool,
    },
}

/// Split `raw_args` at the first bare `--` token into cx-side and agent-side args.
///
/// `raw_args[0]` (the program name) always stays on the cx side. The `--` token
/// itself is dropped; everything after it is returned verbatim so leading-`-` agent
/// flags survive. Returns whether a `--` was present.
fn split_at_first_dash_dash(raw_args: &[String]) -> (Vec<String>, Vec<String>, bool) {
    let dash = raw_args
        .iter()
        .skip(1)
        .position(|arg| arg == "--")
        .map(|i| i + 1);
    match dash {
        Some(i) => {
            let cx_args = raw_args[..i].to_vec();
            let agent_args = raw_args[i + 1..].to_vec();
            (cx_args, agent_args, true)
        }
        None => (raw_args.to_vec(), Vec::new(), false),
    }
}

fn dispatch_command(raw_args: &[String]) -> Result<DispatchCommand> {
    let (cx_args, agent_args, had_dash) = split_at_first_dash_dash(raw_args);
    match Cli::try_parse_from(&cx_args) {
        Ok(cli) => match cli.command {
            Some(CxCommand::Help) => Ok(DispatchCommand::Help),
            Some(CxCommand::Add) => Ok(DispatchCommand::Add),
            Some(CxCommand::Patch {
                source,
                url,
                refresh,
            }) => Ok(DispatchCommand::Patch {
                source,
                url,
                refresh,
            }),
            Some(CxCommand::Probe {
                provider,
                auto_probe,
            }) => Ok(DispatchCommand::Probe {
                provider,
                auto_probe,
            }),
            Some(CxCommand::Stats {
                output,
                view,
                period,
            }) => Ok(DispatchCommand::Stats {
                output,
                view,
                period,
            }),
            Some(CxCommand::Send {
                session,
                clear_buffer,
                text,
            }) => Ok(DispatchCommand::Send {
                session,
                clear_buffer,
                text,
            }),
            Some(CxCommand::Web { port, no_open }) => Ok(DispatchCommand::Web { port, no_open }),
            None => Ok(DispatchCommand::Launch {
                args: agent_args,
                pty: cli.pty,
                socket: cli.socket,
                cwd: cli.cwd,
            }),
        },
        Err(e) => match e.kind() {
            clap::error::ErrorKind::DisplayHelp
            | clap::error::ErrorKind::DisplayVersion
            | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => e.exit(),
            _ => {
                if had_dash {
                    // `--` already separated cx flags from agent args, so a parse error
                    // here is a genuine cx-side flag error (e.g. unknown `--foo`).
                    bail!("{e}");
                }
                bail!(
                    "cx 不再直接接受 agent 名或未知参数；请用 `cx -- <agent> [args]`，\
                     或 `cx --pty -- <agent> [args]`（详见 `cx --help`）"
                );
            }
        },
    }
}

// ══════════════════════════════════════════════════
// 入口
// ══════════════════════════════════════════════════

pub fn run() -> Result<()> {
    let raw_args: Vec<String> = env::args().collect();
    match dispatch_command(&raw_args)? {
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
        DispatchCommand::Probe {
            provider,
            auto_probe,
        } => {
            let config = load_config()?;
            run_probe(provider, auto_probe, &config)
        }
        DispatchCommand::Stats {
            output,
            view,
            period,
        } => {
            let output_format = output
                .map(|value| {
                    stats::OutputFormat::parse(&value).ok_or_else(|| {
                        anyhow!("invalid --output `{value}`; expected one of: svg, png, jpg")
                    })
                })
                .transpose()?;
            let view = view
                .map(|value| {
                    stats::StatsView::parse(&value).ok_or_else(|| {
                        anyhow!("invalid --view `{value}`; expected one of: overview, race")
                    })
                })
                .transpose()?;
            let period = period
                .map(|value| {
                    stats::StatsPeriod::parse(&value).ok_or_else(|| {
                        anyhow!(
                            "invalid --period `{value}`; expected format [n]d, for example: 7d, 10d"
                        )
                    })
                })
                .transpose()?;
            let config = stats::StatsOutputConfig {
                output_format,
                view,
                period,
            };
            stats::run_stats(config)
        }
        DispatchCommand::Send {
            session,
            clear_buffer,
            text,
        } => run_send(session, clear_buffer, text),
        DispatchCommand::Web { port, no_open } => run_web(port, no_open),
        DispatchCommand::Launch {
            args,
            pty,
            socket,
            cwd,
        } => {
            // `--` separated cx flags from agent args; the first arg, if it names a
            // known agent, is consumed as the hint and dropped from passthrough.
            let config = load_config()?;

            if let Some(first) = args.first()
                && let Some(agent) = find_agent(&config, first)
            {
                return run_launcher(
                    Some(agent.id),
                    &config,
                    args[1..].to_vec(),
                    pty,
                    socket,
                    cwd,
                );
            }

            run_launcher(None, &config, args, pty, socket, cwd)
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
    pty: bool,
    socket: Option<String>,
    cwd: Option<PathBuf>,
) -> Result<()> {
    let rerun_agent_hint = agent_hint.clone();
    let rerun_cwd = cwd.clone();
    let mut all_models = build_all_models(config);
    apply_probe_cache(&mut all_models);

    let selection = run_tui(agent_hint, config, &all_models)?;

    let Some(selection) = selection else {
        return Ok(());
    };

    if selection.provider.name == ADD_PROVIDER_SENTINEL {
        if run_add()? {
            let refreshed = load_config()?;
            return run_launcher(
                rerun_agent_hint,
                &refreshed,
                passthrough_args,
                pty,
                socket,
                rerun_cwd,
            );
        }
        return Ok(());
    }

    // ChatGPT.app 走专门的启动 + renderer 注入路径，不经通用 build_launch_spec/launch_agent。
    // 它是 GUI detach，不接管终端，--pty/--socket 对它无意义。
    if selection.agent_id == "ChatGPT.app" {
        if pty {
            eprintln!("cx: --pty 对 ChatGPT.app 无效（GUI detach，不经 PTY 中继）");
        }
        if socket.is_some() {
            eprintln!("cx: --socket 对 ChatGPT.app 无效（GUI detach，无 IPC 注入）");
        }
        let apikey = resolve_chatgpt_app_apikey_interactive(&selection.provider)?;
        apply_selected_model_tab_name(&selection)?;
        let chatgpt_settings = config.chatgpt_app.clone().unwrap_or_default();
        return chatgpt_app::launch_with_injection(
            &selection,
            &apikey,
            &passthrough_args,
            &chatgpt_settings,
        );
    }

    // VS Code 走专门的进程 env 注入路径（VSCODE_CLI=1），不经通用
    // build_launch_spec/launch_agent。GUI detach，不接管终端，--pty/--socket 无意义。
    if selection.agent_id == "VS Code" {
        if pty {
            eprintln!("cx: --pty 对 VS Code 无效（GUI detach，不经 PTY 中继）");
        }
        if socket.is_some() {
            eprintln!("cx: --socket 对 VS Code 无效（GUI detach，无 IPC 注入）");
        }
        apply_selected_model_tab_name(&selection)?;
        return launch_vscode_app_from_settings(None);
    }

    let spec = build_launch_spec(&selection, &passthrough_args, pty, socket, cwd)?;

    apply_selected_model_tab_name(&selection)?;
    launch_agent(spec)
}

/// `cx send`：向运行中的 cx agent session 注入一条消息。
///
/// 薄封装：把 `--session` 字符串解析成 `SendSelector`，委托 `send::send` 完成扫描/连接/写入。
fn run_send(session: Option<String>, clear_buffer: bool, text: Option<String>) -> Result<()> {
    let selector = send::parse_selector(session.as_deref());
    let target = send::send(&selector, text.as_deref(), clear_buffer)?;
    println!("已注入到 session {} ({})", target.id, target.agent);
    Ok(())
}

/// Start the WebUI server and optionally open a browser tab.
fn run_web(_port: Option<u16>, no_open: bool) -> Result<()> {
    let rt = tokio::runtime::Runtime::new().context("failed to build web runtime")?;
    rt.block_on(async move {
        manox_agent::runtime::init();
        manox_webui::spawn_server();
        manox_webui::start_server();

        // Wait for the server to start and obtain the URL.
        let url = loop {
            if let Some(url) = manox_webui::service_url() {
                break url;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        };

        println!("WebUI 服务已启动: {url}");
        if !no_open && let Err(e) = open::that(&url) {
            eprintln!("警告: 无法打开浏览器: {e:#}");
        }

        tokio::signal::ctrl_c().await.context("等待 Ctrl+C 失败")?;
        println!("\nWebUI 服务已关闭");
        Ok(())
    })
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
    Ok(cx_state_dir()?.join(".patch_source"))
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
    probe::runtime().block_on(async_run_patch(source, url, refresh))
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
        // chatgpt_app 段不属于 provider patch 的内容：来源文件显式带有时覆盖，
        // 否则保留本机既有设置。
        chatgpt_app: incoming
            .chatgpt_app
            .clone()
            .or(existing.chatgpt_app.clone()),
        // vscode_app 段同 chatgpt_app 语义。
        vscode_app: incoming.vscode_app.clone().or(existing.vscode_app.clone()),
        subagents: merge_subagents(&existing.subagents, &incoming.subagents),
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

/// Merge the top-level `subagents:` map per key: incoming entries override
/// their own keys, local entries the patch source does not mention survive.
/// The map is host-owned pinning (manox subagent dispatch consumes it), not
/// patch content — a patch author carrying their own `subagents:` block must
/// never silently erase unrelated local pins. An empty map — whether the key
/// is omitted or written explicitly as `subagents: {}` — cannot express
/// "clear all"; clearing stays a manual edit.
fn merge_subagents(
    existing: &BTreeMap<String, String>,
    incoming: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut merged = existing.clone();
    for (key, value) in incoming {
        merged.insert(key.clone(), value.clone());
    }
    merged
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
    if provider.is_remote_models() {
        bail!(
            "Provider `{}` 使用远程 models 接口，不支持手动添加 model",
            provider.name
        );
    }
    if let Some(map) = provider.models_map()
        && map.contains_key(&model_id)
    {
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
            let provider_name = provider.name.clone();
            let map = provider.models_map_mut().with_context(|| {
                format!("Provider `{provider_name}` 使用远程 models 接口，不支持手动添加 model",)
            })?;
            map.insert(model_id.clone(), model);
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

fn launch_agent(spec: LaunchSpec) -> Result<()> {
    let mut command = Command::new(&spec.program);
    command.args(&spec.args);
    for name in &spec.env_remove {
        command.env_remove(name);
    }
    command.envs(&spec.env);
    if let Some(cwd) = spec.cwd.as_deref() {
        command.current_dir(cwd);
    }

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

    // Warp 集成：在启动 agent 前发出 session_start 事件
    let warp_session = warp::maybe_emit_session_start(&spec.agent_id, spec.model_id.as_deref());

    // PTY 中继路径（opt-in，`cx --pty`）：cx 持 master，终端 IO 透传，并暴露 IPC 注入入口。
    // relay::run 自行打印摘要、spawn、进 raw mode、收尾，返回 `!`。
    // Ctrl+C 在 raw mode 下作为 0x03 透传给 slave，由 agent 自处理，cx 不再被
    // SIGINT 杀掉（直连 status() 路径的老限制）。未传 --pty 时 spec.pty 为 false，走下方直连。
    // `warp_session` 按值传入：relay 路径拥有配对的 stop 事件；该分支发散（`-> !`），
    // 故直连路径仍可继续借用 warp_session。
    if spec.pty {
        relay::run(&spec, warp_session);
    }

    // 将 Warp session ID 传递给子进程，以便 agent 的 hooks/plugins
    // 可以使用同一 session ID 发出后续 OSC 777 事件。
    if let Some(ref session) = warp_session {
        command.env("CX_WARP_SESSION_ID", session.session_id());
    }

    // 打印启动摘要
    println!();
    println!("{}", spec.summary);
    println!();

    let started_at = std::time::Instant::now();
    let started_sys = std::time::SystemTime::now();

    // 直连路径（默认，未传 --pty）：spawn + wait，子进程继承 stdin/stdout/stderr，
    // cx 作为静默父进程等待。
    //
    // 已知限制：若用户在 agent 运行时按 Ctrl+C，Rust 默认 SIGINT handler 会立即终止 cx
    // 进程，此时退出摘要和 Warp stop 事件不会触发。PTY 中继路径（`cx --pty`）规避此问题。
    let status = command
        .status()
        .with_context(|| format!("启动 `{}` 失败", spec.program.display()))?;

    finalize_agent_exit(
        &spec.agent_id,
        &spec.provider_name,
        spec.model_id.as_deref(),
        &status,
        started_at,
        started_sys,
        &warp_session,
        &std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    );
}

/// 子进程退出后的统一收尾：提取 token 用量、打印退出摘要、发出 Warp stop 事件、
/// 按子进程退出码退出 cx。供 `launch_agent`（同步 spawn+wait）与 `chatgpt_app` 注入路径
/// （spawn → CDP 注入 → wait）共用，避免 Warp 集成与退出摘要逻辑分叉。
#[allow(clippy::too_many_arguments)]
fn finalize_agent_exit(
    agent_id: &str,
    provider_name: &str,
    model_id: Option<&str>,
    status: &std::process::ExitStatus,
    started_at: std::time::Instant,
    started_sys: std::time::SystemTime,
    warp_session: &Option<warp::WarpSession>,
    cwd: &Path,
) -> ! {
    let exit_code = exit_code_from(status);
    let termination = format_exit_status(status);
    finalize_exit_common(
        agent_id,
        provider_name,
        model_id,
        started_at,
        started_sys,
        exit_code,
        termination.as_deref(),
        warp_session,
        cwd,
        None,
    )
}

/// Shared exit path for both the legacy `Command::status()` launch and the PTY
/// relay: print the exit summary, emit the Warp stop event, clean up an optional
/// IPC session, then exit with the given code.
///
/// `session_id` is `Some` only on the relay path, where it owns an IPC socket +
/// registry file that must be removed before exit.
#[allow(clippy::too_many_arguments)]
pub(crate) fn finalize_exit_common(
    agent_id: &str,
    provider_name: &str,
    model_id: Option<&str>,
    started_at: std::time::Instant,
    started_sys: std::time::SystemTime,
    exit_code: i32,
    termination: Option<&str>,
    warp_session: &Option<warp::WarpSession>,
    cwd: &Path,
    session_id: Option<&str>,
) -> ! {
    let duration = started_at.elapsed();

    // 从 agent 日志中提取本次会话的 token 用量
    let tokens = stats::count_recent_session_tokens(agent_id, started_sys, cwd);

    println!();
    println!(
        "{}",
        format_exit_summary_inline(
            agent_id,
            provider_name,
            model_id,
            duration,
            termination,
            tokens.as_ref(),
        )
    );
    println!();

    // Warp 集成：agent 退出后发出 stop 事件（WarpSession 的 Drop 也会兜底）
    if let Some(session) = warp_session {
        session.emit_stop(Some(exit_code));
    }

    // Relay path owns an IPC socket + registry; remove them so they don't linger.
    if let Some(id) = session_id {
        session::cleanup_session(id);
    }

    std::process::exit(exit_code);
}

/// `format_exit_summary` 的字段版本，供不持有完整 `LaunchSpec` 的调用方（chatgpt_app 注入路径）复用。
fn format_exit_summary_inline(
    agent_id: &str,
    provider_name: &str,
    model_id: Option<&str>,
    duration: std::time::Duration,
    termination: Option<&str>,
    tokens: Option<&stats::SessionTokens>,
) -> String {
    let dur_str = format_duration(duration);
    let mut msg = format!(
        "退出 {} | Provider: {} | {}",
        agent_id,
        provider_name,
        match model_id {
            Some(m) => format!("Model: {m}"),
            None => "Model: default".into(),
        },
    );
    msg.push_str(" | ");
    msg.push_str(&dur_str);
    if let Some(t) = tokens {
        let total = t.total();
        if total > 0 {
            msg.push_str(" | ");
            msg.push_str(&stats::format_tokens_compact(total));
            msg.push_str(" Tokens");
        }
    }
    if let Some(term) = termination {
        msg.push_str(&format!(" | {term}"));
    }
    msg
}

/// 从 `ExitStatus` 计算 cx 退出码：正常退出用 exit code，信号终止用 128+signal。
fn exit_code_from(status: &std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status.signal().map(|s| 128 + s).unwrap_or(1)
    }
    #[cfg(not(unix))]
    {
        let _ = status;
        1
    }
}

/// 将 `ExitStatus` 转换为人类可读的终止描述。
///
/// - 正常退出 (code 0): `None`（不显示）
/// - 非零退出码: `Some("exit 1")`
/// - 信号终止 (Unix): `Some("signal 9")`
fn format_exit_status(status: &std::process::ExitStatus) -> Option<String> {
    #[cfg(unix)]
    if let Some(sig) = status.signal() {
        return Some(format!("signal {sig}"));
    }
    match status.code() {
        Some(0) => None,
        Some(code) => Some(format!("exit {code}")),
        None => None,
    }
}

/// 格式化退出摘要，包含 agent 信息、会话时长和 token 用量。
///
/// 示例：`退出 claude | Provider: 百炼 | Model: MiniMax-M2.7 | 3m12s | 123k Tokens`
///
/// 仅测试使用（生产路径走 `format_exit_summary_inline`），故 gate 在 `cfg(test)` 下避免 release 死代码告警。
#[cfg(test)]
fn format_exit_summary(
    spec: &LaunchSpec,
    duration: std::time::Duration,
    termination: Option<&str>,
    tokens: Option<&stats::SessionTokens>,
) -> String {
    format_exit_summary_inline(
        &spec.agent_id,
        &spec.provider_name,
        spec.model_id.as_deref(),
        duration,
        termination,
        tokens,
    )
}

/// 将时长格式化为人类友好的简短表示。
///
/// - < 1 分钟: "45s"
/// - < 1 小时: "3m12s"
/// - ≥ 1 小时: "1h5m"
fn format_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        let m = secs / 60;
        let s = secs % 60;
        if s == 0 {
            format!("{m}m")
        } else {
            format!("{m}m{s}s")
        }
    } else {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        if m == 0 {
            format!("{h}h")
        } else {
            format!("{h}h{m}m")
        }
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
    /// Agent 标识符（如 "claude"、"codex"、"copilot"），供 Warp 集成和退出摘要使用。
    agent_id: String,
    /// Provider 名称，供退出摘要使用。
    provider_name: String,
    /// 选中的模型 ID，供 Warp 集成和退出摘要使用。
    model_id: Option<String>,
    /// 是否经 PTY 中继启动（cx 持 master，终端 IO 透传），而非 `Command::status()` 直连。
    /// 由 `--pty` CLI flag 决定；默认 false（直连）。
    pty: bool,
    /// 自定义 IPC socket 路径（仅 pty 时生效；None 则用 ~/.manox/sessions/<id>.sock）。
    socket: Option<String>,
    /// Agent 进程的工作目录。`None` 时继承 cx 自身的 cwd（`std::env::current_dir`），
    /// 保留直连/库调用的默认行为；`Some` 时 PTY 与直连两条 spawn 路径都以此为子进程 cwd。
    cwd: Option<PathBuf>,
}

/// Resolve an explicit (provider, model) launch selection to the concrete
/// endpoint variant plus the wire api to launch with. An explicit wire pins
/// the endpoint variant and must be agent-supported; without one the first
/// agent-visible registration wins and the wire falls back to the first
/// agent-supported wire the model offers, then the endpoint's own wire.
pub(crate) fn resolve_launch_model(
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

fn build_launch_spec(
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
        // The wire-facing id is the base id (context suffix stripped; providers
        // do not recognize cx's suffix). ctx_hint ([1m] → 1_000_000) is consumed
        // per agent branch: claude writes CLAUDE_CODE_MAX_CONTEXT_TOKENS, copilot
        // COPILOT_PROVIDER_MAX_PROMPT_TOKENS, codex model_context_window.
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
                // 透传上下文后缀：copilot BYOK 用 COPILOT_PROVIDER_MAX_PROMPT_TOKENS
                // 声明输入上下文窗口（覆盖其内置 catalog 与默认 128K）。
                // 见 `copilot help providers`：token 限制解析顺序为
                // manual env vars → built-in catalog → defaults。
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
                // [1m] context declaration travels with the injection: LaunchSpec
                // env overrides the inherited env at spawn, and suffix-less models
                // leave any shell-exported value untouched (apply_byok_env parity).
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
                // ChatGPT.app 不走通用 LaunchSpec 流程；run_launcher 已分流到 chatgpt_app::launch_with_injection。
                // 此处仅在误入时给出明确错误，避免静默走 generic passthrough。
                bail!("ChatGPT.app 应由注入路径启动，不应进入 build_launch_spec");
            }
            "VS Code" => {
                // VS Code 不走通用 LaunchSpec 流程；run_launcher 已分流到 vscode_app::launch。
                // 此处仅在误入时给出明确错误，避免静默走 generic passthrough。
                bail!("VS Code 应由注入路径启动，不应进入 build_launch_spec");
            }
            _ => {
                // Generic fallback: just pass through
                args.extend(passthrough_args.iter().cloned());
            }
        }
    }

    // Agent 级别环境变量（最低优先级）
    env.extend(
        selection
            .agent_env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone())),
    );

    // Provider + Model 级别环境变量。
    // ResolvedModel.env 已在 from_config 中合并 provider + model env（model 优先），
    // 无 model 时回落为 provider env。均覆盖 agent 同名变量。
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
        // PTY 中继 opt-in：仅当传入 --pty 时启用，否则 build_launch_spec 调用方走直连 status()。
        pty,
        socket,
        cwd,
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
        models: ProviderModels::Inline(BTreeMap::new()),
        endpoints,
        env: BTreeMap::new(),
    };

    if add_model_now {
        match collect_model_draft(terminal, config, &provider, first_wire_api)? {
            PromptOutcome::Submit((model_id, model)) => {
                if let Some(map) = provider.models_map_mut() {
                    map.insert(model_id, model);
                }
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

    Ok(PromptOutcome::Submit((
        model_id,
        ProviderModelConfig {
            desc: empty_string_as_none(&desc),
            wire_apis: vec![wire_api.display().to_string()],
            agents: model_agents,
            env: BTreeMap::new(),
            ..Default::default()
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
            .iter()
            .find(|a| !a.hidden)
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

    /// 用户可见的 agent 列表（过滤掉 hidden agent）。
    fn visible_agents(&self) -> Vec<ResolvedAgent> {
        self.resolved_agents()
            .into_iter()
            .filter(|a| !a.hidden)
            .collect()
    }

    fn agent_wire_apis(&self) -> Vec<WireApi> {
        self.resolved_agents()
            .into_iter()
            .find(|a| a.id == self.selected_agent_id)
            .map(|a| a.supported_wire_apis)
            .unwrap_or_default()
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
                .visible_agents()
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
                .map(|model| {
                    model.formatted_row(&self.model_wire_api_indexes, Some(&self.agent_wire_apis()))
                })
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

        let current = option
            .selected_variant_index(&self.model_wire_api_indexes, Some(&self.agent_wire_apis()));
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
                let agents = self.visible_agents();
                if self.agent_index >= agents.len() {
                    return None;
                }
                let agent = &agents[self.agent_index];
                self.selected_agent_id = agent.id.clone();
                // VS Code 跳过 Provider/Model 选择步：启动读取持久化
                // vscode_app 设置统一注入（单一入口，无选择交互）。
                if agent.id == "VS Code" {
                    return Some(Selection {
                        agent_id: agent.id.clone(),
                        agent_binary: agent.binary.clone(),
                        agent_args: agent.args.clone(),
                        agent_env: agent.env.clone(),
                        selected_wire_api: WireApi::Anthropic,
                        provider: ResolvedProvider {
                            name: String::new(),
                            has_endpoints: false,
                            apikey_source: None,
                            env: BTreeMap::new(),
                        },
                        model: None,
                        injected_models: Vec::new(),
                    });
                }
                self.provider_index = 0;
                self.model_index = 0;
                self.step = Step::Provider;
                None
            }
            Step::Provider => {
                let providers = providers_for_agent(&self.config, &self.selected_agent_id);
                let provider = providers[self.provider_index].clone();
                // ChatGPT.app 跳过 Model 选择步：直接把该 provider 下所有 Responses 模型
                // 作为完整列表注入桌面端，由 cx 在启动时经 CDP 注入 renderer。
                if self.selected_agent_id == "ChatGPT.app" {
                    let injected = injected_models_for_chatgpt_app(models, &provider.name);
                    let agent = find_agent(&self.config, &self.selected_agent_id).unwrap();
                    return Some(Selection {
                        agent_id: agent.id.clone(),
                        agent_binary: agent.binary.clone(),
                        agent_args: agent.args.clone(),
                        agent_env: agent.env.clone(),
                        selected_wire_api: WireApi::Responses,
                        provider,
                        model: injected.first().cloned(),
                        injected_models: injected,
                    });
                }
                if provider.requires_model() {
                    self.model_index = 0;
                    self.step = Step::Model;
                    None
                } else {
                    let agent = find_agent(&self.config, &self.selected_agent_id).unwrap();
                    let agent_wire_apis = self.agent_wire_apis();
                    let selected_wire_api = agent_wire_apis
                        .first()
                        .copied()
                        .unwrap_or(WireApi::Unavailable);
                    Some(Selection {
                        agent_id: agent.id.clone(),
                        agent_binary: agent.binary.clone(),
                        agent_args: agent.args.clone(),
                        agent_env: agent.env.clone(),
                        selected_wire_api,
                        provider,
                        model: None,
                        injected_models: Vec::new(),
                    })
                }
            }
            Step::Model => {
                let providers = providers_for_agent(&self.config, &self.selected_agent_id);
                let provider = providers[self.provider_index].clone();
                let available = self.current_model_options(models);
                let option = available.get(self.model_index)?;
                let selected_variant = option
                    .selected_variant(&self.model_wire_api_indexes, Some(&self.agent_wire_apis()))
                    .clone();
                let agent = find_agent(&self.config, &self.selected_agent_id).unwrap();
                let agent_wire_apis = self.agent_wire_apis();
                let mut selected_wire_api = WireApi::Unavailable;
                for aw in &agent_wire_apis {
                    if selected_variant.model_wire_apis.contains(aw) {
                        selected_wire_api = *aw;
                        break;
                    }
                }
                Some(Selection {
                    agent_id: agent.id.clone(),
                    agent_binary: agent.binary.clone(),
                    agent_args: agent.args.clone(),
                    agent_env: agent.env.clone(),
                    selected_wire_api,
                    provider,
                    model: Some(selected_variant),
                    injected_models: Vec::new(),
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
    let highlight = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);

    if state.step == Step::Model {
        // Model 步：先渲染 block（边框 + 标题），再在 inner area 中渲染表头 + 列表。
        // 表头始终可见、不可选中，列表由外层 block 提供边框。
        let block = Block::default()
            .borders(Borders::ALL)
            .title(current_title(state));
        let inner = block.inner(layout[2]);
        frame.render_widget(block, layout[2]);

        let header_rect = Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: 1,
        };
        let list_rect = Rect {
            x: inner.x,
            y: inner.y + 1,
            width: inner.width,
            height: inner.height.saturating_sub(1),
        };

        let header = Paragraph::new(model_header_row());
        frame.render_widget(header, header_rect);

        let list = List::new(list_items)
            .highlight_style(highlight)
            .highlight_symbol("✨ ");
        frame.render_stateful_widget(list, list_rect, &mut list_state);
    } else {
        // Agent / Provider 步：原有 List + Block 渲染逻辑不变。
        let list = List::new(list_items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(current_title(state)),
            )
            .highlight_style(highlight)
            .highlight_symbol("✨ ");
        frame.render_stateful_widget(list, layout[2], &mut list_state);
    }

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
                models: ProviderModels::Inline(BTreeMap::new()),
                endpoints: BTreeMap::new(),
                env: BTreeMap::new(),
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
            chatgpt_app: None,
            vscode_app: None,
            subagents: Default::default(),
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
            desc: String::new(),
            wire_api,
            model_wire_apis: vec![wire_api],
            provider_name: "DashScope".into(),
            endpoint_url: endpoint_url.into(),
            visible_agents: vec!["codex".into(), "claude".into()],
            copilot_auth: CopilotAuth::ApiKey,
            env: BTreeMap::new(),
            apikey_source: None,
            max_tokens: None,
            context: None,
            supports_tools: true,
            supports_images: false,
        }
    }

    fn multi_wire_api_test_config() -> CxConfig {
        CxConfig {
            providers: vec![ProviderConfig {
                name: "Xiaomi MIMO".into(),
                apikey_source: Some("literal:test".into()),
                models: ProviderModels::Inline(BTreeMap::from([(
                    "mimo-v2.5-pro".into(),
                    ProviderModelConfig {
                        desc: Some("thinking".into()),
                        wire_apis: vec![],
                        agents: Vec::new(),
                        env: BTreeMap::new(),
                        ..Default::default()
                    },
                )])),
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
                env: BTreeMap::new(),
            }],
            agents: default_agent_configs(),
            chatgpt_app: None,
            vscode_app: None,
            subagents: Default::default(),
        }
    }

    // ── clap CLI parsing tests ──

    fn parse(args: &[&str]) -> Option<CxCommand> {
        Cli::try_parse_from(std::iter::once("cx").chain(args.iter().copied()))
            .ok()
            .and_then(|cli| cli.command)
    }

    fn dispatch(args: &[&str]) -> Result<DispatchCommand> {
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
        assert_eq!(
            dispatch(&[]).unwrap(),
            DispatchCommand::Launch {
                args: Vec::new(),
                pty: false,
                socket: None,
                cwd: None,
            }
        );
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
    fn stats_dispatch_preserves_raw_period_value_for_validation() {
        assert_eq!(
            dispatch(&["stats", "--period", "10d", "--output", "jpg"]).unwrap(),
            DispatchCommand::Stats {
                output: Some("jpg".into()),
                view: None,
                period: Some("10d".into()),
            }
        );
        assert_eq!(
            dispatch(&["stats", "--period", "31d"]).unwrap(),
            DispatchCommand::Stats {
                output: None,
                view: None,
                period: Some("31d".into()),
            }
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
        assert_eq!(dispatch(&["help"]).unwrap(), DispatchCommand::Help);
    }

    #[test]
    fn dispatch_bare_agent_name_errors() {
        // `cx claude mcp list` (no `--`) is no longer tolerated — the agent name
        // must come after `--`.
        assert!(dispatch(&["claude", "mcp", "list"]).is_err());
    }

    #[test]
    fn dispatch_dash_dash_separates_pty_and_agent_args() {
        // `cx --pty -- claude --foo` → pty on, args = ["claude", "--foo"]
        assert_eq!(
            dispatch(&["--pty", "--", "claude", "--foo"]).unwrap(),
            DispatchCommand::Launch {
                args: vec!["claude".into(), "--foo".into()],
                pty: true,
                socket: None,
                cwd: None,
            }
        );
        // `cx --pty -- --foo` → TUI agent selection, passthrough = ["--foo"]
        assert_eq!(
            dispatch(&["--pty", "--", "--foo"]).unwrap(),
            DispatchCommand::Launch {
                args: vec!["--foo".into()],
                pty: true,
                socket: None,
                cwd: None,
            }
        );
        // `cx -- claude` → pty off (direct), args = ["claude"]
        assert_eq!(
            dispatch(&["--", "claude"]).unwrap(),
            DispatchCommand::Launch {
                args: vec!["claude".into()],
                pty: false,
                socket: None,
                cwd: None,
            }
        );
        // `cx --pty` (no agent args) → pty on, TUI selection
        assert_eq!(
            dispatch(&["--pty"]).unwrap(),
            DispatchCommand::Launch {
                args: Vec::new(),
                pty: true,
                socket: None,
                cwd: None,
            }
        );
    }

    #[test]
    fn dispatch_socket_flag_requires_pty_and_is_threaded() {
        // `cx --pty -S /tmp/cx.sock -- claude` → custom socket threaded into Launch.
        assert_eq!(
            dispatch(&["--pty", "-S", "/tmp/cx.sock", "--", "claude"]).unwrap(),
            DispatchCommand::Launch {
                args: vec!["claude".into()],
                pty: true,
                socket: Some("/tmp/cx.sock".into()),
                cwd: None,
            }
        );
        // `--socket` without `--pty` is rejected by clap (requires = "pty").
        assert!(dispatch(&["--socket", "/tmp/cx.sock", "--", "claude"]).is_err());
        // `--socket` after `--` is an agent arg, not a cx flag.
        assert_eq!(
            dispatch(&["--pty", "--", "--socket", "/tmp/x"]).unwrap(),
            DispatchCommand::Launch {
                args: vec!["--socket".into(), "/tmp/x".into()],
                pty: true,
                socket: None,
                cwd: None,
            }
        );
    }

    #[test]
    fn canonicalize_agent_name_is_case_insensitive_with_aliases() {
        assert_eq!(canonicalize_agent_name("CoDex.App"), "ChatGPT.app");
        assert_eq!(canonicalize_agent_name("codex.app"), "ChatGPT.app");
        assert_eq!(canonicalize_agent_name("CODEXAPP"), "ChatGPT.app");
        assert_eq!(canonicalize_agent_name("codex_app"), "ChatGPT.app");
        assert_eq!(canonicalize_agent_name("ChatGPT.App"), "ChatGPT.app");
        assert_eq!(canonicalize_agent_name("chatgpt.app"), "ChatGPT.app");
        assert_eq!(canonicalize_agent_name("CHATGPTAPP"), "ChatGPT.app");
        assert_eq!(canonicalize_agent_name("chatgpt_app"), "ChatGPT.app");
        assert_eq!(canonicalize_agent_name("Claude"), "claude");
        assert_eq!(canonicalize_agent_name("vscode"), "VS Code");
        assert_eq!(canonicalize_agent_name("VS Code"), "VS Code");
        assert_eq!(canonicalize_agent_name("vs-code"), "VS Code");
        assert_eq!(canonicalize_agent_name("VS_CODE"), "VS Code");
    }

    #[test]
    fn build_launch_spec_rejects_vscode_agent() {
        // VS Code 与 ChatGPT.app 一样是注入型 agent：run_launcher 分流，
        // build_launch_spec 误入时必须显式报错而非静默 passthrough。
        let fake_binary = create_fake_binary("claude");
        let selection = Selection {
            agent_id: "VS Code".into(),
            agent_binary: fake_binary.display().to_string(),
            agent_args: vec!["vscode".into()],
            agent_env: BTreeMap::new(),
            selected_wire_api: WireApi::Anthropic,
            provider: ResolvedProvider {
                name: "DashScope".into(),
                has_endpoints: true,
                apikey_source: Some("literal:test-key".into()),
                env: BTreeMap::new(),
            },
            model: Some(ResolvedModel {
                id: "qwen3.7-max".into(),
                desc: String::new(),
                wire_api: WireApi::Anthropic,
                model_wire_apis: vec![WireApi::Anthropic],
                provider_name: "DashScope".into(),
                endpoint_url: "https://dashscope.aliyuncs.com/apps/anthropic".into(),
                visible_agents: vec!["claude".into(), "VS Code".into()],
                copilot_auth: CopilotAuth::ApiKey,
                env: BTreeMap::new(),
                apikey_source: None,
                max_tokens: None,
                context: None,
                supports_tools: true,
                supports_images: false,
            }),
            injected_models: Vec::new(),
        };
        let err = build_launch_spec(&selection, &[], false, None, None)
            .expect_err("VS Code 不应进入 build_launch_spec");
        assert!(err.to_string().contains("VS Code"));
        let _ = fs::remove_dir_all(fake_binary.parent().unwrap());
    }

    #[test]
    fn build_vscode_selection_resolves_provider_and_model() {
        let config: CxConfig = r#"
providers:
- name: test
  apikey_source: literal:k
  models:
    m1:
      wire_apis: [anthropic]
  endpoints:
    anthropic:
      url: https://example.com/anthropic
agents:
- id: claude
  binary: claude
  wire_apis: [anthropic]
"#
        .parse()
        .expect("parse");
        let all_models = config.resolve_all_models();
        let selection =
            build_vscode_selection(&config, &all_models, "test", "m1").expect("selection");
        assert_eq!(selection.agent_id, "VS Code");
        assert_eq!(selection.selected_wire_api, WireApi::Anthropic);
        assert_eq!(selection.provider.name, "test");
        assert_eq!(selection.model.as_ref().unwrap().id, "m1");
        assert!(selection.injected_models.is_empty());

        // 未知 provider / model 明确报错
        assert!(build_vscode_selection(&config, &all_models, "nope", "m1").is_err());
        assert!(build_vscode_selection(&config, &all_models, "test", "nope").is_err());
    }

    /// 双 provider 测试配置：`both` 同时有 Anthropic（b1/b2）与 Responses（b1）
    /// 模型；`anthropic-only` 仅 Anthropic（a1）。
    fn vscode_settings_test_config() -> CxConfig {
        r#"
providers:
- name: both
  apikey_source: literal:k
  models:
    b2:
      wire_apis: [anthropic]
    b1:
      wire_apis: [anthropic, responses]
  endpoints:
    anthropic:
      url: https://example.com/anthropic
    responses:
      url: https://example.com/responses
- name: anthropic-only
  apikey_source: literal:k
  models:
    a1:
      wire_apis: [anthropic]
  endpoints:
    anthropic:
      url: https://example.com/anthropic2
agents:
- id: claude
  binary: claude
  wire_apis: [anthropic]
"#
        .parse()
        .expect("parse")
    }

    #[test]
    fn injected_models_for_vscode_claude_sorts_and_filters() {
        let config = vscode_settings_test_config();
        let all_models = config.resolve_all_models();
        let ids: Vec<String> = injected_models_for_vscode_claude(&all_models, "both")
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(ids, vec!["b1".to_string(), "b2".to_string()]);
        let ids: Vec<String> = injected_models_for_vscode_claude(&all_models, "anthropic-only")
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(ids, vec!["a1".to_string()]);
    }

    #[test]
    fn resolve_vscode_claude_part_defaults_and_overrides() {
        let config = vscode_settings_test_config();
        let all_models = config.resolve_all_models();

        // 默认块 → 第一个兼容 provider（配置序）+ 首个 id 序模型
        let part =
            resolve_vscode_claude_part(&config, &all_models, &VsCodeExtensionBlock::default())
                .expect("resolve")
                .expect("part");
        assert_eq!(part.selection.provider.name, "both");
        assert_eq!(part.selection.model.as_ref().unwrap().id, "b1");
        assert_eq!(part.apikey, "k");

        // 显式 provider
        let part = resolve_vscode_claude_part(
            &config,
            &all_models,
            &VsCodeExtensionBlock {
                provider: Some("anthropic-only".into()),
                disabled: false,
            },
        )
        .expect("resolve")
        .expect("part");
        assert_eq!(part.selection.provider.name, "anthropic-only");
        assert_eq!(part.selection.model.as_ref().unwrap().id, "a1");

        // 未知 provider → 回落第一个兼容候选
        let part = resolve_vscode_claude_part(
            &config,
            &all_models,
            &VsCodeExtensionBlock {
                provider: Some("ghost".into()),
                disabled: false,
            },
        )
        .expect("resolve")
        .expect("part");
        assert_eq!(part.selection.provider.name, "both");

        // disabled → 不注入
        assert!(
            resolve_vscode_claude_part(
                &config,
                &all_models,
                &VsCodeExtensionBlock {
                    provider: None,
                    disabled: true,
                },
            )
            .expect("resolve")
            .is_none()
        );
    }

    #[test]
    fn pick_vscode_provider_semantics() {
        let mk = |name: &str| {
            (
                ResolvedProvider {
                    name: name.into(),
                    has_endpoints: true,
                    apikey_source: None,
                    env: BTreeMap::new(),
                },
                vec![ResolvedModel {
                    id: "m".into(),
                    desc: String::new(),
                    wire_api: WireApi::Responses,
                    model_wire_apis: vec![WireApi::Responses],
                    provider_name: name.into(),
                    endpoint_url: "https://example.com".into(),
                    visible_agents: vec![],
                    copilot_auth: CopilotAuth::ApiKey,
                    env: BTreeMap::new(),
                    apikey_source: None,
                    max_tokens: None,
                    context: None,
                    supports_tools: true,
                    supports_images: false,
                }],
            )
        };
        let candidates = vec![mk("p1"), mk("p2")];

        // 未配置 → 第一个候选
        assert_eq!(
            pick_vscode_provider(&candidates, None, "t").map(|(p, _)| p.name.as_str()),
            Some("p1")
        );
        // 显式命中
        assert_eq!(
            pick_vscode_provider(&candidates, Some("p2"), "t").map(|(p, _)| p.name.as_str()),
            Some("p2")
        );
        // 未知 → 回落第一个
        assert_eq!(
            pick_vscode_provider(&candidates, Some("ghost"), "t").map(|(p, _)| p.name.as_str()),
            Some("p1")
        );
        // 空候选 → None
        let empty: Vec<(ResolvedProvider, Vec<ResolvedModel>)> = Vec::new();
        assert!(pick_vscode_provider(&empty, None, "t").is_none());
    }

    #[test]
    fn tui_confirm_vscode_short_circuits_at_agent_step() {
        let config = vscode_settings_test_config();
        let models = config.resolve_all_models();
        let mut state = AppState::new(None, &config);
        let agents = state.visible_agents();
        let idx = agents
            .iter()
            .position(|a| a.id == "VS Code")
            .expect("VS Code agent 应可见");
        state.agent_index = idx;
        let selection = state
            .confirm(&models)
            .expect("Agent 步选中 VS Code 应立即返回哨兵 Selection");
        assert_eq!(selection.agent_id, "VS Code");
        assert!(selection.model.is_none());
        assert!(selection.provider.name.is_empty());
        assert_eq!(state.step, Step::Agent);
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
            selected_wire_api: WireApi::Responses,
            provider: ResolvedProvider {
                name: "Codex Default".into(),
                has_endpoints: false,
                apikey_source: None,
                env: BTreeMap::new(),
            },
            model: None,
            injected_models: Vec::new(),
        };

        let spec = build_launch_spec(
            &selection,
            &["mcp".into(), "serve".into()],
            false,
            None,
            None,
        )
        .unwrap();
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
            selected_wire_api: WireApi::Responses,
            provider: ResolvedProvider {
                name: "Test".into(),
                has_endpoints: false,
                apikey_source: Some("literal:test-key".into()),
                env: BTreeMap::new(),
            },
            model: None,
            injected_models: Vec::new(),
        };
        let spec = build_launch_spec(&selection, &[], false, None, None).unwrap();
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
            selected_wire_api: WireApi::Responses,
            provider: ResolvedProvider {
                name: "DashScope".into(),
                has_endpoints: true,
                apikey_source: Some("literal:test-key".into()),
                env: BTreeMap::new(),
            },
            model: Some(ResolvedModel {
                id: "qwen3.6-plus".into(),
                desc: String::new(),
                wire_api: WireApi::Anthropic,
                model_wire_apis: vec![WireApi::Anthropic],
                provider_name: "DashScope".into(),
                endpoint_url: "https://dashscope.aliyuncs.com/apps/anthropic".into(),
                visible_agents: vec!["claude".into()],
                copilot_auth: CopilotAuth::ApiKey,
                env: BTreeMap::new(),
                apikey_source: None,
                max_tokens: None,
                context: None,
                supports_tools: true,
                supports_images: false,
            }),
            injected_models: Vec::new(),
        };
        let spec = build_launch_spec(
            &selection,
            &["mcp".into(), "list".into()],
            false,
            None,
            None,
        )
        .unwrap();
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
        // Suffix-less model: no context declaration injected.
        assert!(!spec.env.contains_key("CLAUDE_CODE_MAX_CONTEXT_TOKENS"));
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
    fn launch_strips_1m_suffix_for_claude() {
        // `glm-5.2[1m]` → claude 收到 ANTHROPIC_MODEL=glm-5.2、--model glm-5.2、
        // CX_MODEL=glm-5.2（[1m] 是 cx 内部上下文后缀，provider 不识别）。
        let fake_binary = create_fake_binary("claude");
        let selection = Selection {
            agent_id: "claude".into(),
            agent_binary: fake_binary.display().to_string(),
            agent_args: Vec::new(),
            agent_env: BTreeMap::new(),
            selected_wire_api: WireApi::Anthropic,
            provider: ResolvedProvider {
                name: "DashScope".into(),
                has_endpoints: true,
                apikey_source: Some("literal:test-key".into()),
                env: BTreeMap::new(),
            },
            model: Some(ResolvedModel {
                id: "glm-5.2[1m]".into(),
                desc: String::new(),
                wire_api: WireApi::Anthropic,
                model_wire_apis: vec![WireApi::Anthropic],
                provider_name: "DashScope".into(),
                endpoint_url: "https://dashscope.aliyuncs.com/apps/anthropic".into(),
                visible_agents: vec!["claude".into()],
                copilot_auth: CopilotAuth::ApiKey,
                env: BTreeMap::new(),
                apikey_source: None,
                max_tokens: None,
                context: None,
                supports_tools: true,
                supports_images: false,
            }),
            injected_models: Vec::new(),
        };
        let spec = build_launch_spec(&selection, &[], false, None, None).unwrap();
        assert_eq!(spec.env.get("CX_MODEL"), Some(&"glm-5.2".to_string()));
        assert_eq!(
            spec.env.get("ANTHROPIC_MODEL"),
            Some(&"glm-5.2".to_string())
        );
        assert!(spec.args.iter().any(|a| a == "glm-5.2"));
        assert!(!spec.args.iter().any(|a| a.contains("[1m]")));
        assert_eq!(
            spec.env.get("CLAUDE_CODE_MAX_CONTEXT_TOKENS"),
            Some(&"1000000".to_string())
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
            selected_wire_api: WireApi::Responses,
            provider: ResolvedProvider {
                name: "Packy API".into(),
                has_endpoints: true,
                apikey_source: Some("literal:test-key".into()),
                env: BTreeMap::new(),
            },
            model: Some(ResolvedModel {
                id: "claude-opus-4-7".into(),
                desc: String::new(),
                wire_api: WireApi::Anthropic,
                model_wire_apis: vec![WireApi::Anthropic],
                provider_name: "Packy API".into(),
                endpoint_url: "https://www.packyapi.com/".into(),
                visible_agents: vec!["copilot".into()],
                copilot_auth: CopilotAuth::BearerToken,
                env: BTreeMap::new(),
                apikey_source: None,
                max_tokens: None,
                context: None,
                supports_tools: true,
                supports_images: false,
            }),
            injected_models: Vec::new(),
        };

        let spec = build_launch_spec(&selection, &[], false, None, None).unwrap();
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
    fn copilot_passes_1m_suffix_as_max_prompt_tokens() {
        // `glm-5.2[1m]` → copilot 收到 COPILOT_MODEL=glm-5.2（后缀剥除），
        // 同时 COPILOT_PROVIDER_MAX_PROMPT_TOKENS=1000000 透传 1M 上下文窗口。
        let fake_binary = create_fake_binary("copilot");
        let selection = Selection {
            agent_id: "copilot".into(),
            agent_binary: fake_binary.display().to_string(),
            agent_args: Vec::new(),
            agent_env: BTreeMap::new(),
            selected_wire_api: WireApi::Responses,
            provider: ResolvedProvider {
                name: "Packy API".into(),
                has_endpoints: true,
                apikey_source: Some("literal:test-key".into()),
                env: BTreeMap::new(),
            },
            model: Some(ResolvedModel {
                id: "glm-5.2[1m]".into(),
                desc: String::new(),
                wire_api: WireApi::Completions,
                model_wire_apis: vec![WireApi::Completions],
                provider_name: "Packy API".into(),
                endpoint_url: "https://www.packyapi.com/".into(),
                visible_agents: vec!["copilot".into()],
                copilot_auth: CopilotAuth::BearerToken,
                env: BTreeMap::new(),
                apikey_source: None,
                max_tokens: None,
                context: None,
                supports_tools: true,
                supports_images: false,
            }),
            injected_models: Vec::new(),
        };

        let spec = build_launch_spec(&selection, &[], false, None, None).unwrap();
        assert_eq!(spec.env.get("COPILOT_MODEL"), Some(&"glm-5.2".to_string()));
        assert_eq!(
            spec.env.get("COPILOT_PROVIDER_MAX_PROMPT_TOKENS"),
            Some(&"1000000".to_string())
        );
        let _ = fs::remove_dir_all(fake_binary.parent().unwrap());
    }

    #[test]
    fn copilot_omits_max_prompt_tokens_without_suffix() {
        // 无上下文后缀时不设 COPILOT_PROVIDER_MAX_PROMPT_TOKENS，
        // 让 copilot 走 catalog / 默认值，行为不变。
        let fake_binary = create_fake_binary("copilot");
        let selection = Selection {
            agent_id: "copilot".into(),
            agent_binary: fake_binary.display().to_string(),
            agent_args: Vec::new(),
            agent_env: BTreeMap::new(),
            selected_wire_api: WireApi::Responses,
            provider: ResolvedProvider {
                name: "Packy API".into(),
                has_endpoints: true,
                apikey_source: Some("literal:test-key".into()),
                env: BTreeMap::new(),
            },
            model: Some(ResolvedModel {
                id: "claude-opus-4-7".into(),
                desc: String::new(),
                wire_api: WireApi::Anthropic,
                model_wire_apis: vec![WireApi::Anthropic],
                provider_name: "Packy API".into(),
                endpoint_url: "https://www.packyapi.com/".into(),
                visible_agents: vec!["copilot".into()],
                copilot_auth: CopilotAuth::BearerToken,
                env: BTreeMap::new(),
                apikey_source: None,
                max_tokens: None,
                context: None,
                supports_tools: true,
                supports_images: false,
            }),
            injected_models: Vec::new(),
        };

        let spec = build_launch_spec(&selection, &[], false, None, None).unwrap();
        assert!(spec.env.contains_key("COPILOT_MODEL"));
        assert!(!spec.env.contains_key("COPILOT_PROVIDER_MAX_PROMPT_TOKENS"));
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
            selected_wire_api: WireApi::Responses,
            provider: ResolvedProvider {
                name: "Default".into(),
                has_endpoints: false,
                apikey_source: None,
                env: BTreeMap::new(),
            },
            model: None,
            injected_models: Vec::new(),
        };

        assert!(apply_selected_model_tab_name(&selection).is_ok());
    }

    // ── ChatGPT.app selection tests ──

    fn chatgpt_test_config() -> CxConfig {
        CxConfig {
            providers: vec![ProviderConfig {
                name: "TestProv".into(),
                apikey_source: Some("literal:test".into()),
                models: ProviderModels::Inline(BTreeMap::from([
                    (
                        "model-a".into(),
                        ProviderModelConfig {
                            wire_apis: vec!["responses".into()],
                            ..Default::default()
                        },
                    ),
                    (
                        "model-b".into(),
                        ProviderModelConfig {
                            wire_apis: vec!["responses".into()],
                            ..Default::default()
                        },
                    ),
                ])),
                endpoints: BTreeMap::from([(
                    "responses".into(),
                    ProviderEndpointSpec::Url("https://example.com".into()),
                )]),
                env: BTreeMap::new(),
            }],
            agents: default_agent_configs(),
            chatgpt_app: None,
            vscode_app: None,
            subagents: Default::default(),
        }
    }

    fn chatgpt_test_models(provider_name: &str) -> Vec<ResolvedModel> {
        ["model-a", "model-b"]
            .into_iter()
            .map(|id| ResolvedModel {
                id: id.into(),
                desc: String::new(),
                wire_api: WireApi::Responses,
                model_wire_apis: vec![WireApi::Responses],
                provider_name: provider_name.into(),
                endpoint_url: "https://example.com".into(),
                visible_agents: vec!["ChatGPT.app".into()],
                copilot_auth: CopilotAuth::ApiKey,
                env: BTreeMap::new(),
                apikey_source: None,
                max_tokens: None,
                context: None,
                supports_tools: true,
                supports_images: false,
            })
            .collect()
    }

    #[test]
    fn build_chatgpt_selection_moves_selected_default_first() {
        let config = chatgpt_test_config();
        let models = chatgpt_test_models("TestProv");
        let selection = build_chatgpt_selection(&config, &models, "TestProv", "model-b").unwrap();
        assert_eq!(selection.agent_id, "ChatGPT.app");
        assert_eq!(selection.selected_wire_api, WireApi::Responses);
        assert_eq!(selection.model.as_ref().unwrap().id, "model-b");
        let ids: Vec<&str> = selection
            .injected_models
            .iter()
            .map(|m| m.id.as_str())
            .collect();
        assert_eq!(ids, vec!["model-b", "model-a"]);
        assert_eq!(selection.provider.name, "TestProv");
    }

    #[test]
    fn build_chatgpt_selection_unknown_provider_errors() {
        let config = chatgpt_test_config();
        let models = chatgpt_test_models("TestProv");
        let err = build_chatgpt_selection(&config, &models, "NoSuchProv", "model-a")
            .unwrap_err()
            .to_string();
        assert!(err.contains("未找到 provider"), "{err}");
    }

    #[test]
    fn build_chatgpt_selection_unknown_model_errors() {
        let config = chatgpt_test_config();
        let models = chatgpt_test_models("TestProv");
        let err = build_chatgpt_selection(&config, &models, "TestProv", "ghost")
            .unwrap_err()
            .to_string();
        assert!(err.contains("注入目录中未找到模型"), "{err}");
    }

    #[test]
    fn build_chatgpt_selection_empty_catalog_errors() {
        let config = chatgpt_test_config();
        let err = build_chatgpt_selection(&config, &[], "TestProv", "model-a")
            .unwrap_err()
            .to_string();
        assert!(err.contains("没有支持 Responses"), "{err}");
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
            selected_wire_api: WireApi::Anthropic,
            provider: ResolvedProvider {
                name: "Test".into(),
                has_endpoints: false,
                apikey_source: Some("literal:test-key".into()),
                env: BTreeMap::new(),
            },
            model: None,
            injected_models: Vec::new(),
        };
        let spec = build_launch_spec(&selection, &[], false, None, None).unwrap();
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
            selected_wire_api: WireApi::Responses,
            provider: ResolvedProvider {
                name: "DashScope".into(),
                has_endpoints: true,
                apikey_source: Some("literal:test-key".into()),
                env: BTreeMap::new(),
            },
            model: Some(ResolvedModel {
                id: "glm-5.1".into(),
                desc: String::new(),
                wire_api: WireApi::Anthropic,
                model_wire_apis: vec![WireApi::Anthropic],
                provider_name: "DashScope".into(),
                endpoint_url: "https://dashscope.aliyuncs.com/apps/anthropic".into(),
                visible_agents: vec!["claude".into()],
                copilot_auth: CopilotAuth::ApiKey,
                env: model_env,
                apikey_source: None,
                max_tokens: None,
                context: None,
                supports_tools: true,
                supports_images: false,
            }),
            injected_models: Vec::new(),
        };
        let spec = build_launch_spec(&selection, &[], false, None, None).unwrap();
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
            selected_wire_api: WireApi::Anthropic,
            provider: ResolvedProvider {
                name: "DashScope".into(),
                has_endpoints: true,
                apikey_source: Some("literal:test-key".into()),
                env: BTreeMap::new(),
            },
            model: Some(ResolvedModel {
                id: "glm-5.1".into(),
                desc: String::new(),
                wire_api: WireApi::Anthropic,
                model_wire_apis: vec![WireApi::Anthropic],
                provider_name: "DashScope".into(),
                endpoint_url: "https://dashscope.aliyuncs.com/apps/anthropic".into(),
                visible_agents: vec!["claude".into()],
                copilot_auth: CopilotAuth::ApiKey,
                env: model_env,
                apikey_source: None,
                max_tokens: None,
                context: None,
                supports_tools: true,
                supports_images: false,
            }),
            injected_models: Vec::new(),
        };
        let spec = build_launch_spec(&selection, &[], false, None, None).unwrap();
        // Model env overrides agent env for the shared key
        assert_eq!(spec.env.get("SHARED_VAR"), Some(&"from-model".into()));
        // Both unique keys are present
        assert_eq!(spec.env.get("AGENT_ONLY_VAR"), Some(&"agent-value".into()));
        assert_eq!(spec.env.get("MODEL_ONLY_VAR"), Some(&"model-value".into()));
        let _ = fs::remove_dir_all(fake_binary.parent().unwrap());
    }

    #[test]
    fn provider_env_is_injected_and_overrides_agent_env() {
        let fake_binary = create_fake_binary("claude");
        let mut agent_env = BTreeMap::new();
        agent_env.insert("SHARED".into(), "from-agent".into());
        agent_env.insert("AGENT_ONLY".into(), "agent".into());

        let mut provider_env = BTreeMap::new();
        provider_env.insert("SHARED".into(), "from-provider".into());
        provider_env.insert("PROVIDER_ONLY".into(), "provider".into());

        let selection = Selection {
            agent_id: "claude".into(),
            agent_binary: fake_binary.display().to_string(),
            agent_args: Vec::new(),
            agent_env,
            selected_wire_api: WireApi::Anthropic,
            provider: ResolvedProvider {
                name: "Test".into(),
                has_endpoints: false,
                apikey_source: Some("literal:test-key".into()),
                env: provider_env,
            },
            model: None,
            injected_models: Vec::new(),
        };
        let spec = build_launch_spec(&selection, &[], false, None, None).unwrap();
        // Provider overrides agent
        assert_eq!(spec.env.get("SHARED"), Some(&"from-provider".into()));
        assert_eq!(spec.env.get("AGENT_ONLY"), Some(&"agent".into()));
        assert_eq!(spec.env.get("PROVIDER_ONLY"), Some(&"provider".into()));
        let _ = fs::remove_dir_all(fake_binary.parent().unwrap());
    }

    #[test]
    fn model_env_overrides_provider_env() {
        let fake_binary = create_fake_binary("claude");
        let mut provider_env = BTreeMap::new();
        provider_env.insert("SHARED".into(), "from-provider".into());
        provider_env.insert("PROVIDER_ONLY".into(), "provider".into());

        let mut model_env = BTreeMap::new();
        model_env.insert("SHARED".into(), "from-model".into());

        let mut merged_env = provider_env.clone();
        merged_env.extend(model_env.clone());

        let selection = Selection {
            agent_id: "claude".into(),
            agent_binary: fake_binary.display().to_string(),
            agent_args: Vec::new(),
            agent_env: BTreeMap::new(),
            selected_wire_api: WireApi::Responses,
            provider: ResolvedProvider {
                name: "DashScope".into(),
                has_endpoints: true,
                apikey_source: Some("literal:test-key".into()),
                env: provider_env,
            },
            model: Some(ResolvedModel {
                id: "glm-5.1".into(),
                desc: String::new(),
                wire_api: WireApi::Anthropic,
                model_wire_apis: vec![WireApi::Anthropic],
                provider_name: "DashScope".into(),
                endpoint_url: "https://example.com/anthropic".into(),
                visible_agents: vec!["claude".into()],
                copilot_auth: CopilotAuth::ApiKey,
                env: merged_env,
                apikey_source: None,
                max_tokens: None,
                context: None,
                supports_tools: true,
                supports_images: false,
            }),
            injected_models: Vec::new(),
        };
        let spec = build_launch_spec(&selection, &[], false, None, None).unwrap();
        // Model overrides provider for shared key
        assert_eq!(spec.env.get("SHARED"), Some(&"from-model".into()));
        // Provider-only key is still present (via merged model.env)
        assert_eq!(spec.env.get("PROVIDER_ONLY"), Some(&"provider".into()));
        let _ = fs::remove_dir_all(fake_binary.parent().unwrap());
    }

    #[test]
    fn resolved_model_env_merges_provider_and_model_env() {
        let mut provider_env = BTreeMap::new();
        provider_env.insert("PROVIDER_VAR".into(), "pv".into());
        provider_env.insert("SHARED".into(), "from-provider".into());

        let mut model_only_env = BTreeMap::new();
        model_only_env.insert("MODEL_VAR".into(), "mv".into());
        model_only_env.insert("SHARED".into(), "from-model".into());

        let provider = ProviderConfig {
            name: "Test".into(),
            apikey_source: None,
            models: ProviderModels::Inline(BTreeMap::from([(
                "m1".into(),
                ProviderModelConfig {
                    desc: None,
                    wire_apis: vec![],
                    agents: Vec::new(),
                    env: model_only_env,
                    ..Default::default()
                },
            )])),
            endpoints: BTreeMap::from([(
                "anthropic".into(),
                ProviderEndpointSpec::Url("https://example.com".into()),
            )]),
            env: provider_env,
        };
        let config = CxConfig {
            providers: vec![provider.clone()],
            agents: default_agent_configs(),
            chatgpt_app: None,
            vscode_app: None,
            subagents: Default::default(),
        };
        let endpoints = provider.normalized_endpoints();
        let model = &endpoints[0].models[0];
        let resolved = resolved_model_from_config(&config, &provider, &endpoints[0], model);

        // Model env overrides provider env for shared key
        assert_eq!(resolved.env.get("SHARED"), Some(&"from-model".into()));
        // Both unique keys are present
        assert_eq!(resolved.env.get("PROVIDER_VAR"), Some(&"pv".into()));
        assert_eq!(resolved.env.get("MODEL_VAR"), Some(&"mv".into()));
    }

    // ── Merge tests ──

    #[test]
    fn merge_providers_replaces_by_name() {
        let existing = vec![ProviderConfig {
            name: "A".into(),
            apikey_source: Some("literal:old".into()),
            models: ProviderModels::Inline(BTreeMap::new()),
            endpoints: BTreeMap::new(),
            env: BTreeMap::new(),
        }];
        let incoming = vec![ProviderConfig {
            name: "A".into(),
            apikey_source: Some("literal:new".into()),
            models: ProviderModels::Inline(BTreeMap::new()),
            endpoints: BTreeMap::new(),
            env: BTreeMap::new(),
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
            models: ProviderModels::Inline(BTreeMap::new()),
            endpoints: BTreeMap::new(),
            env: BTreeMap::new(),
        }];
        let incoming = vec![ProviderConfig {
            name: "B".into(),
            apikey_source: None,
            models: ProviderModels::Inline(BTreeMap::new()),
            endpoints: BTreeMap::new(),
            env: BTreeMap::new(),
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
                models: ProviderModels::Inline(BTreeMap::new()),
                endpoints: BTreeMap::new(),
                env: BTreeMap::new(),
            },
            ProviderConfig {
                name: "B".into(),
                apikey_source: Some("literal:old".into()),
                models: ProviderModels::Inline(BTreeMap::new()),
                endpoints: BTreeMap::new(),
                env: BTreeMap::new(),
            },
            ProviderConfig {
                name: "C".into(),
                apikey_source: Some("literal:c".into()),
                models: ProviderModels::Inline(BTreeMap::new()),
                endpoints: BTreeMap::new(),
                env: BTreeMap::new(),
            },
        ];
        let incoming = vec![
            ProviderConfig {
                name: "B".into(),
                apikey_source: Some("literal:new".into()),
                models: ProviderModels::Inline(BTreeMap::new()),
                endpoints: BTreeMap::new(),
                env: BTreeMap::new(),
            },
            ProviderConfig {
                name: "D".into(),
                apikey_source: Some("literal:d".into()),
                models: ProviderModels::Inline(BTreeMap::new()),
                endpoints: BTreeMap::new(),
                env: BTreeMap::new(),
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

    fn btree(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn merge_subagents_keeps_local_when_incoming_empty() {
        let existing = btree(&[("Explore", "百炼::qwen3.8-flash::high")]);
        let merged = merge_subagents(&existing, &BTreeMap::new());
        assert_eq!(merged, existing);
    }

    #[test]
    fn merge_subagents_overrides_per_key_and_keeps_unrelated_local() {
        let existing = btree(&[("Sailor", "DeepSeek::deepseek-v4-flash::high")]);
        let incoming = btree(&[("Explore", "百炼::glm-5.3::medium")]);
        let merged = merge_subagents(&existing, &incoming);
        assert_eq!(merged.len(), 2);
        assert_eq!(
            merged.get("Sailor").map(String::as_str),
            Some("DeepSeek::deepseek-v4-flash::high")
        );
        assert_eq!(
            merged.get("Explore").map(String::as_str),
            Some("百炼::glm-5.3::medium")
        );
    }

    #[test]
    fn merge_subagents_incoming_wins_on_same_key() {
        let existing = btree(&[("Explore", "百炼::glm-5.3::high")]);
        let incoming = btree(&[("Explore", "百炼::qwen3.8-flash::high")]);
        let merged = merge_subagents(&existing, &incoming);
        assert_eq!(merged.len(), 1);
        assert_eq!(
            merged.get("Explore").map(String::as_str),
            Some("百炼::qwen3.8-flash::high")
        );
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
            models: ProviderModels::Inline(BTreeMap::new()),
            endpoints: BTreeMap::from([(
                "anthropic".into(),
                ProviderEndpointSpec::Url("https://example.com/anthropic".into()),
            )]),
            env: BTreeMap::new(),
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
                    desc: Some("Agent/终端最强".into()),
                    wire_apis: vec!["responses".into()],
                    agents: vec!["codex".into()],
                    env: BTreeMap::new(),
                    ..Default::default()
                },
            },
        )
        .unwrap();

        assert!(matches!(result, AddResult::Model { .. }));
        let stored = config.providers[0]
            .models_map()
            .unwrap()
            .get("qwen3.6-plus")
            .unwrap();
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
                models: ProviderModels::Inline(BTreeMap::from([(
                    "claude-opus-4-7".into(),
                    ProviderModelConfig {
                        desc: None,
                        wire_apis: vec!["anthropic".into()],
                        agents: Vec::new(),
                        env: BTreeMap::new(),
                        ..Default::default()
                    },
                )])),
                endpoints: BTreeMap::from([(
                    "anthropic".into(),
                    ProviderEndpointSpec::Url("https://example.com/anthropic".into()),
                )]),
                env: BTreeMap::new(),
            }],
            agents: default_agent_configs(),
            chatgpt_app: None,
            vscode_app: None,
            subagents: Default::default(),
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
                .formatted_row(&BTreeMap::new(), None)
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
    fn provider_config_key_slugifies_names() {
        assert_eq!(provider_config_key("DashScope"), "dashscope");
        assert_eq!(provider_config_key("Packy API"), "packyapi");
        assert_eq!(provider_config_key("百炼"), "custom");
        assert_eq!(provider_config_key("My-Provider_1"), "my-provider_1");
    }

    #[test]
    fn env_key_for_apikey_source_extracts_var_name() {
        assert_eq!(
            env_key_for_apikey_source(Some("keychain:DASHSCOPE_API_KEY")),
            "DASHSCOPE_API_KEY"
        );
        assert_eq!(env_key_for_apikey_source(Some("env:MY_KEY")), "MY_KEY");
        assert_eq!(
            env_key_for_apikey_source(Some("literal:abc")),
            "CX_PROVIDER_KEY"
        );
        assert_eq!(env_key_for_apikey_source(None), "CX_PROVIDER_KEY");
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
            WireApi::Responses,
            "dashscope",
            "DashScope",
            "DASHSCOPE_API_KEY",
            "qwen3.6-plus",
            None,
            None,
        )
        .unwrap();

        assert!(merged.contains(r#"model = "qwen3.6-plus""#));
        assert!(merged.contains(r#"base_url = "https://dashscope.aliyuncs.com/v1""#));
        assert!(merged.contains(r#"[projects."/tmp/workspace"]"#));
        assert!(merged.contains(r#"trust_level = "trusted""#));
        assert!(merged.contains(r#"approval_policy = "on-request""#));
        assert!(merged.contains(r#"[projects."/tmp/other"]"#));
        assert!(merged.contains(r#"model_reasoning_effort = "high""#));
        assert!(merged.contains(r#"env_key = "DASHSCOPE_API_KEY""#));
        assert!(!merged.contains("https://old.example.com"));
        // 旧的 dashscope section 应被剥离，由动态 provider key 重新生成
        assert!(merged.contains(r#"model_provider = "dashscope""#));
    }

    #[test]
    fn codex_wire_api_str_maps_to_codex_family_vocabulary() {
        // ChatGPT.app 引擎的 config.toml 使用 responses / chat_completions / anthropic_messages，
        // 而非 cx 内部 copilot 用的 completions / anthropic。
        assert_eq!(codex_wire_api_str(WireApi::Responses).unwrap(), "responses");
        assert_eq!(
            codex_wire_api_str(WireApi::Completions).unwrap(),
            "chat_completions"
        );
        assert_eq!(
            codex_wire_api_str(WireApi::Anthropic).unwrap(),
            "anthropic_messages"
        );
        assert!(codex_wire_api_str(WireApi::Unavailable).is_err());
    }

    #[test]
    fn merge_codex_config_writes_codex_family_wire_api() {
        // config.toml 必须写出 codex 家族词汇（responses / chat_completions / anthropic_messages）。
        let merged = merge_codex_config(
            None,
            &test_resolved_model(
                "claude-sonnet",
                "https://api.anthropic.com",
                WireApi::Anthropic,
            ),
            Path::new("/tmp/workspace"),
            WireApi::Anthropic,
            "anthropic",
            "Anthropic",
            "ANTHROPIC_API_KEY",
            "claude-sonnet",
            None,
            None,
        )
        .unwrap();
        assert!(merged.contains(r#"wire_api = "anthropic_messages""#));

        let merged = merge_codex_config(
            None,
            &test_resolved_model("gpt-4o", "https://api.openai.com/v1", WireApi::Completions),
            Path::new("/tmp/workspace"),
            WireApi::Completions,
            "openai",
            "OpenAI",
            "OPENAI_API_KEY",
            "gpt-4o",
            None,
            None,
        )
        .unwrap();
        assert!(merged.contains(r#"wire_api = "chat_completions""#));
    }

    #[test]
    fn merge_codex_config_strips_1m_suffix_and_writes_context_window() {
        // glm-5.2[1m] → 发给 codex 的 model 是 glm-5.2；1m 上下文写入 model_context_window。
        let merged = merge_codex_config(
            None,
            &test_resolved_model("glm-5.2[1m]", "https://dashscope/v1", WireApi::Responses),
            Path::new("/tmp/workspace"),
            WireApi::Responses,
            "bailian",
            "Bailian",
            "DASHSCOPE_API_KEY",
            "glm-5.2",
            Some(1_000_000),
            None,
        )
        .unwrap();
        assert!(merged.contains(r#"model = "glm-5.2""#));
        assert!(!merged.contains("glm-5.2[1m]"));
        assert!(merged.contains("model_context_window = 1000000"));
    }

    #[test]
    fn merge_codex_config_no_suffix_leaves_context_window_absent() {
        // 无上下文后缀时不写 model_context_window，也不误删用户既有值。
        let existing = "model_context_window = 200000\napproval_policy = \"never\"\n";
        let merged = merge_codex_config(
            Some(existing),
            &test_resolved_model("glm-5.1", "https://dashscope/v1", WireApi::Responses),
            Path::new("/tmp/workspace"),
            WireApi::Responses,
            "bailian",
            "Bailian",
            "DASHSCOPE_API_KEY",
            "glm-5.1",
            None,
            None,
        )
        .unwrap();
        assert!(merged.contains(r#"model = "glm-5.1""#));
        // 用户既有的 context window 被保留（cx 未覆盖）。
        assert!(merged.contains("model_context_window = 200000"));
    }

    #[test]
    fn merge_codex_config_suffix_overrides_user_context_window() {
        // 有上下文后缀时 cx 重写 model_context_window，剥离用户旧值。
        let existing = "model_context_window = 200000\n";
        let merged = merge_codex_config(
            Some(existing),
            &test_resolved_model("glm-5.2[1m]", "https://dashscope/v1", WireApi::Responses),
            Path::new("/tmp/workspace"),
            WireApi::Responses,
            "bailian",
            "Bailian",
            "DASHSCOPE_API_KEY",
            "glm-5.2",
            Some(1_000_000),
            None,
        )
        .unwrap();
        assert!(merged.contains("model_context_window = 1000000"));
        assert!(!merged.contains("model_context_window = 200000"));
        // 只出现一次。
        assert_eq!(
            merged.matches("model_context_window").count(),
            1,
            "model_context_window 不应重复"
        );
    }

    #[test]
    fn merge_codex_config_writes_model_catalog_json_when_provided() {
        let merged = merge_codex_config(
            None,
            &test_resolved_model(
                "deepseek-v4-pro[1m]",
                "https://api.deepseek.com/v1",
                WireApi::Responses,
            ),
            Path::new("/tmp/workspace"),
            WireApi::Responses,
            "deepseek",
            "DeepSeek",
            "DEEPSEEK_API_KEY",
            "deepseek-v4-pro",
            Some(1_000_000),
            Some("/tmp/cx-home/.codex/model-catalog.json"),
        )
        .unwrap();
        assert!(
            merged.contains(r#"model_catalog_json = "/tmp/cx-home/.codex/model-catalog.json""#)
        );
        // 只出现一次。
        assert_eq!(
            merged.matches("model_catalog_json").count(),
            1,
            "model_catalog_json 不应重复"
        );
    }

    #[test]
    fn merge_codex_config_strips_user_catalog_when_rewriting() {
        // cx 重写 model_catalog_json 时，剥离用户旧值以免冲突。
        let existing = "model_catalog_json = \"/old/path.json\"\n";
        let merged = merge_codex_config(
            Some(existing),
            &test_resolved_model(
                "deepseek-v4-pro[1m]",
                "https://api.deepseek.com/v1",
                WireApi::Responses,
            ),
            Path::new("/tmp/workspace"),
            WireApi::Responses,
            "deepseek",
            "DeepSeek",
            "DEEPSEEK_API_KEY",
            "deepseek-v4-pro",
            Some(1_000_000),
            Some("/tmp/cx-home/.codex/model-catalog.json"),
        )
        .unwrap();
        assert!(
            merged.contains(r#"model_catalog_json = "/tmp/cx-home/.codex/model-catalog.json""#)
        );
        assert!(!merged.contains("/old/path.json"));
    }

    #[test]
    fn parse_model_context_suffix_handles_known_shapes() {
        assert_eq!(
            parse_model_context_suffix("glm-5.2[1m]"),
            ("glm-5.2", Some(1_000_000))
        );
        assert_eq!(
            parse_model_context_suffix("model[3m]"),
            ("model", Some(3_000_000))
        );
        assert_eq!(parse_model_context_suffix("gpt-4o"), ("gpt-4o", None));
        // 不匹配的尾缀原样保留。
        assert_eq!(
            parse_model_context_suffix("model[1mm]"),
            ("model[1mm]", None)
        );
        assert_eq!(parse_model_context_suffix("[1m]"), ("", Some(1_000_000)));
        // Unified parser handles more formats: [200k], [1M], [1m123k], etc.
        assert_eq!(
            parse_model_context_suffix("model[200k]"),
            ("model", Some(200_000))
        );
        assert_eq!(
            parse_model_context_suffix("model[1M]"),
            ("model", Some(1_000_000))
        );
        assert_eq!(
            parse_model_context_suffix("model[1m123k]"),
            ("model", Some(1_123_000))
        );
    }

    #[test]
    fn merge_codex_config_preserves_user_reasoning_effort() {
        // 用户偏好 low，cx 重写时应保留而非硬编码 high。
        let existing = "model_reasoning_effort = \"low\"\n";
        let merged = merge_codex_config(
            Some(existing),
            &test_resolved_model("qwen3.6-plus", "https://example.com/v1", WireApi::Responses),
            Path::new("/tmp/workspace"),
            WireApi::Responses,
            "custom",
            "Custom",
            "CX_PROVIDER_KEY",
            "qwen3.6-plus",
            None,
            None,
        )
        .unwrap();
        assert!(merged.contains(r#"model_reasoning_effort = "low""#));
        assert!(!merged.contains(r#"model_reasoning_effort = "high""#));
        // 不应出现重复的 reasoning effort 行
        assert_eq!(
            merged.matches("model_reasoning_effort").count(),
            1,
            "reasoning effort 行不应重复"
        );
    }

    #[test]
    fn merge_codex_config_defaults_reasoning_effort_to_high_when_absent() {
        let merged = merge_codex_config(
            None,
            &test_resolved_model("qwen3.6-plus", "https://example.com/v1", WireApi::Responses),
            Path::new("/tmp/workspace"),
            WireApi::Responses,
            "custom",
            "Custom",
            "CX_PROVIDER_KEY",
            "qwen3.6-plus",
            None,
            None,
        )
        .unwrap();
        assert!(merged.contains(r#"model_reasoning_effort = "high""#));
    }

    #[test]
    fn inject_supports_websockets_inserts_after_wire_api() {
        let config = "model = \"m\"\nmodel_provider = \"p\"\n\n[model_providers.\"p\"]\n\
                      name = \"P\"\nbase_url = \"https://example.com\"\nenv_key = \"K\"\n\
                      wire_api = \"responses\"\n\n[projects.\"/w\"]\n\ntrust_level = \"trusted\"\n";
        let injected = inject_supports_websockets(config, false);
        assert!(injected.contains("wire_api = \"responses\"\nsupports_websockets = false\n"));
        // 注入行只出现一次，且其后内容（projects 段）原样保留。
        assert_eq!(injected.matches("supports_websockets").count(), 1);
        assert!(injected.contains("[projects.\"/w\"]"));
        assert!(injected.contains("trust_level = \"trusted\""));
    }

    #[test]
    fn inject_supports_websockets_no_marker_returns_original() {
        let config = "model = \"m\"\n";
        assert_eq!(inject_supports_websockets(config, true), config);
    }

    #[test]
    fn chatgpt_reserved_env_keys_include_dynamic_key() {
        let keys = chatgpt_reserved_env_keys("DASHSCOPE_API_KEY");
        assert!(keys.contains(&"CODEX_HOME".to_string()));
        assert!(keys.contains(&"CX_MODEL".to_string()));
        assert!(keys.contains(&"CX_WARP_SESSION_ID".to_string()));
        assert!(keys.contains(&"DASHSCOPE_API_KEY".to_string()));
    }

    #[test]
    fn validate_reserved_keys_superset_launch_filter() {
        // 不变量：保存端校验的保留键集合 ⊇ 启动端过滤集合，
        // 即「保存通过 → 启动必不丢弃」。对每个 provider 的 env_key 成立。
        let config = minimal_test_config();
        let all = all_reserved_env_keys(&config);
        for provider in &config.providers {
            let env_key = env_key_for_apikey_source(provider.apikey_source.as_deref());
            for key in chatgpt_reserved_env_keys(&env_key) {
                assert!(
                    all.contains(&key),
                    "save-side validation must cover launch-side reserved key {key}"
                );
            }
        }
    }

    #[test]
    fn normalize_chatgpt_app_settings_drops_false_keeps_true() {
        let off = ChatGptAppSettings {
            supports_websockets: Some(false),
            ..Default::default()
        };
        assert_eq!(
            normalize_chatgpt_app_settings(off).supports_websockets,
            None,
            "Some(false) 应归一为 None（与默认等价、整段可省略）"
        );
        let on = ChatGptAppSettings {
            supports_websockets: Some(true),
            ..Default::default()
        };
        assert_eq!(
            normalize_chatgpt_app_settings(on).supports_websockets,
            Some(true),
            "Some(true) 必须保留"
        );
    }

    #[test]
    fn normalize_chatgpt_app_settings_trims_blank_nickname() {
        let blank = ChatGptAppSettings {
            nickname: Some("   ".into()),
            ..Default::default()
        };
        assert_eq!(
            normalize_chatgpt_app_settings(blank).nickname,
            None,
            "纯空白昵称应归一为 None（整段可省略）"
        );
        let trimmed = ChatGptAppSettings {
            nickname: Some(" 我的模型 ".into()),
            ..Default::default()
        };
        assert_eq!(
            normalize_chatgpt_app_settings(trimmed).nickname.as_deref(),
            Some("我的模型"),
            "昵称两侧空白应去除"
        );
    }

    #[test]
    fn chatgpt_provider_display_name_prefers_nickname() {
        let unnamed = ChatGptAppSettings::default();
        assert_eq!(
            chatgpt_provider_display_name("百炼", &unnamed),
            "百炼",
            "未配置昵称时保持 provider 本名"
        );
        let blank = ChatGptAppSettings {
            nickname: Some("  ".into()),
            ..Default::default()
        };
        assert_eq!(
            chatgpt_provider_display_name("百炼", &blank),
            "百炼",
            "空白昵称视为未配置"
        );
        let named = ChatGptAppSettings {
            nickname: Some("我的模型".into()),
            ..Default::default()
        };
        assert_eq!(
            chatgpt_provider_display_name("百炼", &named),
            "我的模型",
            "配置昵称后无论哪个 provider 都替换为昵称"
        );
    }

    #[test]
    fn extract_reasoning_effort_strips_quotes_and_ignores_empty() {
        assert_eq!(
            extract_reasoning_effort(Some("model_reasoning_effort = \"medium\"")),
            Some("medium".to_string())
        );
        assert_eq!(
            extract_reasoning_effort(Some("model_reasoning_effort=high")),
            Some("high".to_string())
        );
        assert_eq!(extract_reasoning_effort(Some("model = \"x\"")), None);
        assert_eq!(extract_reasoning_effort(None), None);
        // 只取顶层键，不误取 [model_providers.*] 段内同名字段
        let cfg = "model = \"qwen\"\n\n[model_providers.foo]\nmodel_reasoning_effort = \"low\"\n";
        assert_eq!(extract_reasoning_effort(Some(cfg)), None);
        // 顶层值存在时正常取，即使后面 section 里也有
        let cfg2 = "model_reasoning_effort = \"high\"\n\n[model_providers.foo]\nmodel_reasoning_effort = \"low\"\n";
        assert_eq!(
            extract_reasoning_effort(Some(cfg2)),
            Some("high".to_string())
        );
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
    fn materialize_passthrough_dir_is_idempotent_on_rerun() {
        // 持久目录（如 ~/.manox/.codex/）二次启动时，上次创建的符号链接已存在，
        // materialize 必须能幂等地重建而非报 EEXIST。
        let root = temp_test_dir("passthrough-idempotent");
        let real = root.join("real");
        let fake = root.join("fake");
        fs::create_dir_all(&real).unwrap();
        fs::write(real.join("logs.sqlite-wal"), "data").unwrap();

        materialize_passthrough_dir(&real, &fake, &["config.toml"]).unwrap();
        // 第二次：symlink 已存在，必须不报错并仍指向真实文件
        materialize_passthrough_dir(&real, &fake, &["config.toml"]).unwrap();

        assert!(fake.join("logs.sqlite-wal").exists());
        #[cfg(unix)]
        assert!(
            fs::symlink_metadata(fake.join("logs.sqlite-wal"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read_to_string(fake.join("logs.sqlite-wal")).unwrap(),
            "data"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn materialize_passthrough_dir_replaces_file_left_by_atomic_rename() {
        // ChatGPT.app 用 atomic-rename 写状态文件时会把我们的符号链接替换成普通文件
        // （如 .codex-global-state.json.bak）。real 目录里有真实数据源，重新符号链接即可。
        let root = temp_test_dir("passthrough-atomic-rename");
        let real = root.join("real");
        let fake = root.join("fake");
        fs::create_dir_all(&real).unwrap();
        fs::write(real.join("state.json.bak"), "real-source").unwrap();
        // fake 里已有一个普通文件（模拟 Codex rename 覆盖了符号链接后的状态）
        fs::create_dir_all(&fake).unwrap();
        fs::write(fake.join("state.json.bak"), "stale-local").unwrap();

        // 之前会报 EEXIST；现在应替换为指向 real 的符号链接
        materialize_passthrough_dir(&real, &fake, &["config.toml"]).unwrap();

        #[cfg(unix)]
        assert!(
            fs::symlink_metadata(fake.join("state.json.bak"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        // 读到的是 real 的数据，而非残留的本地文件
        assert_eq!(
            fs::read_to_string(fake.join("state.json.bak")).unwrap(),
            "real-source"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn materialize_passthrough_dir_preserves_real_directory_at_dst() {
        // 若 dst 已是真实目录（非符号链接），不强行删除以免误删 Codex 状态目录。
        let root = temp_test_dir("passthrough-real-dir");
        let real = root.join("real");
        let fake = root.join("fake");
        fs::create_dir_all(&real).unwrap();
        fs::create_dir_all(real.join("sessions")).unwrap();
        fs::write(real.join("sessions").join("a.json"), "session").unwrap();
        // fake 里 sessions 已是真实目录（模拟 Codex 在 CODEX_HOME 自建目录）
        fs::create_dir_all(fake.join("sessions")).unwrap();
        fs::write(fake.join("sessions").join("local.json"), "local").unwrap();

        materialize_passthrough_dir(&real, &fake, &["config.toml"]).unwrap();
        // 真实目录被保留，未被替换为符号链接、未被清空
        assert!(
            fs::symlink_metadata(fake.join("sessions"))
                .unwrap()
                .file_type()
                .is_dir()
        );
        assert!(fake.join("sessions").join("local.json").exists());

        let _ = fs::remove_dir_all(&root);
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
            chatgpt_app: None,
            vscode_app: None,
            subagents: Default::default(),
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

    #[test]
    fn format_duration_seconds_only() {
        assert_eq!(format_duration(Duration::from_secs(0)), "0s");
        assert_eq!(format_duration(Duration::from_secs(45)), "45s");
        assert_eq!(format_duration(Duration::from_secs(59)), "59s");
    }

    #[test]
    fn format_duration_minutes_and_seconds() {
        assert_eq!(format_duration(Duration::from_secs(60)), "1m");
        assert_eq!(format_duration(Duration::from_secs(90)), "1m30s");
        assert_eq!(format_duration(Duration::from_secs(192)), "3m12s");
        assert_eq!(format_duration(Duration::from_secs(3599)), "59m59s");
    }

    #[test]
    fn format_duration_hours_and_minutes() {
        assert_eq!(format_duration(Duration::from_secs(3600)), "1h");
        assert_eq!(format_duration(Duration::from_secs(3660)), "1h1m");
        assert_eq!(format_duration(Duration::from_secs(7200)), "2h");
        assert_eq!(format_duration(Duration::from_secs(5400)), "1h30m");
    }

    #[test]
    fn format_exit_summary_with_model() {
        let spec = LaunchSpec {
            program: PathBuf::from("/usr/bin/claude"),
            args: vec![],
            env: BTreeMap::new(),
            summary: String::new(),
            detach: false,
            env_remove: vec![],
            agent_id: "claude".into(),
            provider_name: "百炼".into(),
            model_id: Some("MiniMax-M2.7".into()),
            pty: false,
            socket: None,
            cwd: None,
        };
        let msg = format_exit_summary(&spec, Duration::from_secs(192), None, None);
        assert_eq!(
            msg,
            "退出 claude | Provider: 百炼 | Model: MiniMax-M2.7 | 3m12s"
        );
    }

    #[test]
    fn format_exit_summary_without_model() {
        let spec = LaunchSpec {
            program: PathBuf::from("/usr/bin/copilot"),
            args: vec![],
            env: BTreeMap::new(),
            summary: String::new(),
            detach: false,
            env_remove: vec![],
            agent_id: "copilot".into(),
            provider_name: "default".into(),
            model_id: None,
            pty: false,
            socket: None,
            cwd: None,
        };
        let msg = format_exit_summary(&spec, Duration::from_secs(45), None, None);
        assert_eq!(
            msg,
            "退出 copilot | Provider: default | Model: default | 45s"
        );
    }

    #[test]
    fn format_exit_summary_nonzero_exit_code() {
        let spec = LaunchSpec {
            program: PathBuf::from("/usr/bin/codex"),
            args: vec![],
            env: BTreeMap::new(),
            summary: String::new(),
            detach: false,
            env_remove: vec![],
            agent_id: "codex".into(),
            provider_name: "DashScope".into(),
            model_id: Some("qwen-max".into()),
            pty: false,
            socket: None,
            cwd: None,
        };
        let msg = format_exit_summary(&spec, Duration::from_secs(10), Some("exit 1"), None);
        assert_eq!(
            msg,
            "退出 codex | Provider: DashScope | Model: qwen-max | 10s | exit 1"
        );
    }

    #[test]
    fn format_exit_summary_signal_killed() {
        let spec = LaunchSpec {
            program: PathBuf::from("/usr/bin/claude"),
            args: vec![],
            env: BTreeMap::new(),
            summary: String::new(),
            detach: false,
            env_remove: vec![],
            agent_id: "claude".into(),
            provider_name: "Anthropic".into(),
            model_id: Some("opus-4.7".into()),
            pty: false,
            socket: None,
            cwd: None,
        };
        let msg = format_exit_summary(&spec, Duration::from_secs(5), Some("signal 9"), None);
        assert_eq!(
            msg,
            "退出 claude | Provider: Anthropic | Model: opus-4.7 | 5s | signal 9"
        );
    }

    #[test]
    fn format_exit_summary_with_tokens() {
        let spec = LaunchSpec {
            program: PathBuf::from("/usr/bin/claude"),
            args: vec![],
            env: BTreeMap::new(),
            summary: String::new(),
            detach: false,
            env_remove: vec![],
            agent_id: "claude".into(),
            provider_name: "百炼".into(),
            model_id: Some("MiniMax-M2.7".into()),
            pty: false,
            socket: None,
            cwd: None,
        };
        let tokens = stats::SessionTokens {
            input: 100_000,
            output: 23_000,
            cache_read: 50_000,
            cache_creation: 10_000,
        };
        let msg = format_exit_summary(&spec, Duration::from_secs(192), None, Some(&tokens));
        assert_eq!(
            msg,
            "退出 claude | Provider: 百炼 | Model: MiniMax-M2.7 | 3m12s | 123k Tokens"
        );
    }

    #[test]
    fn format_tokens_compact_cases() {
        assert_eq!(stats::format_tokens_compact(0), "0");
        assert_eq!(stats::format_tokens_compact(500), "500");
        assert_eq!(stats::format_tokens_compact(1_000), "1k");
        assert_eq!(stats::format_tokens_compact(1_500), "1.5k");
        assert_eq!(stats::format_tokens_compact(12_300), "12k");
        assert_eq!(stats::format_tokens_compact(123_000), "123k");
        assert_eq!(stats::format_tokens_compact(1_000_000), "1m");
        assert_eq!(stats::format_tokens_compact(3_123_000), "3m123k");
        assert_eq!(stats::format_tokens_compact(10_500_000), "10m500k");
    }

    /// 向前兼容测试：旧配置文件中含已移除的 lcb_pro 字段时，
    /// Removed fields (`swe_pro`, `hle`, `context_window`, `lcb_pro`) in old YAML are
    /// silently ignored by serde (no `deny_unknown_fields`).
    #[test]
    fn deserialization_ignores_removed_benchmark_fields() {
        let old_yaml = r#"
providers:
  - name: Test
    apikey_source: "literal:test"
    models:
      test-model:
        swe_pro: "56.6%"
        lcb_pro: "1226"
        hle: "28.8%"
        context_window: "1M"
        desc: "test"
        wire_apis: [responses]
agents:
  - id: claude
    bin: claude
    wire_apis: [anthropic]
"#;
        let config: CxConfig = serde_yaml::from_str(old_yaml).unwrap();
        let model = config.providers[0]
            .models_map()
            .unwrap()
            .get("test-model")
            .unwrap();
        assert_eq!(model.desc.as_deref(), Some("test"));
        // swe_pro / hle / context / lcb_pro are no longer struct fields;
        // serde silently ignores unknown keys in old YAML.
    }

    #[test]
    fn provider_env_is_deserialized_from_yaml() {
        let yaml = r#"
providers:
  - name: 百炼
    apikey_source: "keychain:DASHSCOPE_API_KEY"
    env:
      ANTHROPIC_DEFAULT_SONNET_MODEL: "qwen3.7-max"
      ANTHROPIC_DEFAULT_HAIKU_MODEL: "qwen3.7-max"
    endpoints:
      anthropic:
        url: https://example.com/anthropic
    models:
      glm-5.1:
        wire_apis: [anthropic]
        env:
          CLAUDE_CODE_AUTO_COMPACT_WINDOW: "1000000"
agents:
  - id: claude
    bin: claude
    wire_apis: [anthropic]
"#;
        let config: CxConfig = serde_yaml::from_str(yaml).unwrap();
        let provider = &config.providers[0];

        // Provider-level env
        assert_eq!(provider.env.len(), 2);
        assert_eq!(
            provider.env.get("ANTHROPIC_DEFAULT_SONNET_MODEL"),
            Some(&"qwen3.7-max".to_string())
        );
        assert_eq!(
            provider.env.get("ANTHROPIC_DEFAULT_HAIKU_MODEL"),
            Some(&"qwen3.7-max".to_string())
        );

        // Model-level env preserved
        let model = provider.models_map().unwrap().get("glm-5.1").unwrap();
        assert_eq!(
            model.env.get("CLAUDE_CODE_AUTO_COMPACT_WINDOW"),
            Some(&"1000000".to_string())
        );

        // ResolvedModel merges provider + model env
        let endpoints = provider.normalized_endpoints();
        let resolved =
            resolved_model_from_config(&config, provider, &endpoints[0], &endpoints[0].models[0]);
        assert_eq!(resolved.env.len(), 3);
        assert_eq!(
            resolved.env.get("ANTHROPIC_DEFAULT_SONNET_MODEL"),
            Some(&"qwen3.7-max".to_string())
        );
        assert_eq!(
            resolved.env.get("CLAUDE_CODE_AUTO_COMPACT_WINDOW"),
            Some(&"1000000".to_string())
        );
    }

    #[test]
    fn provider_env_model_env_conflict_model_wins() {
        let yaml = r#"
providers:
  - name: Test
    env:
      SHARED_VAR: "from-provider"
      PROVIDER_ONLY: "yes"
    endpoints:
      anthropic:
        url: https://example.com
    models:
      m1:
        env:
          SHARED_VAR: "from-model"
          MODEL_ONLY: "yes"
agents:
  - id: claude
    bin: claude
    wire_apis: [anthropic]
"#;
        let config: CxConfig = serde_yaml::from_str(yaml).unwrap();
        let provider = &config.providers[0];
        let endpoints = provider.normalized_endpoints();
        let resolved =
            resolved_model_from_config(&config, provider, &endpoints[0], &endpoints[0].models[0]);

        assert_eq!(
            resolved.env.get("SHARED_VAR"),
            Some(&"from-model".to_string())
        );
        assert_eq!(resolved.env.get("PROVIDER_ONLY"), Some(&"yes".to_string()));
        assert_eq!(resolved.env.get("MODEL_ONLY"), Some(&"yes".to_string()));
    }

    fn wire_test_agent(id: &str, wires: &[WireApi]) -> ResolvedAgent {
        ResolvedAgent {
            id: id.into(),
            binary: id.into(),
            args: Vec::new(),
            supported_wire_apis: wires.to_vec(),
            env: BTreeMap::new(),
            hidden: false,
        }
    }

    /// One config model registered through every wire endpoint; `visible_agents`
    /// narrowed to copilot (the only built-in seeing all three wires).
    fn wire_variant_model(wire: WireApi) -> ResolvedModel {
        let mut m = test_resolved_model("m", "https://p.example", wire);
        m.provider_name = "P".into();
        m.visible_agents = vec!["copilot".into()];
        m.model_wire_apis = vec![WireApi::Anthropic, WireApi::Responses, WireApi::Completions];
        m
    }

    #[test]
    fn resolve_launch_model_explicit_wire_pins_variant() {
        let models = vec![
            wire_variant_model(WireApi::Anthropic),
            wire_variant_model(WireApi::Completions),
        ];
        let agent = wire_test_agent(
            "copilot",
            &[WireApi::Anthropic, WireApi::Responses, WireApi::Completions],
        );
        let (model, selected) =
            resolve_launch_model(&models, "P", &agent, "m", Some(WireApi::Completions)).unwrap();
        assert_eq!(model.wire_api, WireApi::Completions);
        assert_eq!(selected, WireApi::Completions);
    }

    #[test]
    fn resolve_launch_model_without_wire_keeps_first_match() {
        let models = vec![
            wire_variant_model(WireApi::Anthropic),
            wire_variant_model(WireApi::Completions),
        ];
        let agent = wire_test_agent(
            "copilot",
            &[WireApi::Anthropic, WireApi::Responses, WireApi::Completions],
        );
        let (model, selected) = resolve_launch_model(&models, "P", &agent, "m", None).unwrap();
        assert_eq!(model.wire_api, WireApi::Anthropic);
        assert_eq!(selected, WireApi::Anthropic);
    }

    #[test]
    fn resolve_launch_model_rejects_agent_unsupported_wire() {
        let models = vec![wire_variant_model(WireApi::Anthropic)];
        let agent = wire_test_agent("claude", &[WireApi::Anthropic]);
        assert!(
            resolve_launch_model(&models, "P", &agent, "m", Some(WireApi::Completions)).is_err()
        );
    }
}
