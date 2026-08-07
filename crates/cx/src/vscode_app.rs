//! VS Code 桌面端启动注入：macOS「工具 → VS Code → provider → model」级联的落点。
//! 入口 [`launch`]（BYOK 注入）与 [`launch_plain`]（普通打开）。
//!
//! 注入机制：先解析用户登录 shell 环境（`$SHELL -i -l` env dump），把 Claude Code
//! BYOK env 以最高优先级叠加其上，再带 `VSCODE_CLI=1` 直接启动 VS Code 二进制。
//! VS Code 检测 `VSCODE_CLI=1` 即跳过自身 shell env 解析（preload 合并顺序
//! `process.env < shellEnv < userEnv`，跳过时 shellEnv 为空），扩展宿主、
//! Claude Code 扩展内置 CLI 与集成终端因此全部继承注入 env——可覆盖
//! `~/.zshrc` 导出的 `ANTHROPIC_BASE_URL` / `ANTHROPIC_API_KEY`。
//! 不写任何 settings.json，API Key 不落盘。
//!
//! VS Code 同一 user-data-dir 的第二实例会转发给已运行实例（env 丢失），因此
//! 注入前若 VS Code 正在运行，先弹确认框请求重启：osascript 优雅退出并等待，
//! 超时（如有未保存对话框未处理）则中止注入、VS Code 保持运行。

use crate::Selection;
use crate::parse_model_context_suffix;
use crate::probe::runtime;
use anyhow::{Context, Result, bail};
use base64::{
    Engine as _, engine::general_purpose::STANDARD, engine::general_purpose::URL_SAFE_NO_PAD,
};
use rand::RngCore;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// VS Code Stable 默认安装路径；`CX_VSCODE_APP` 可覆盖（.app bundle 或内部二进制）。
const DEFAULT_APP_PATH: &str = "/Applications/Visual Studio Code.app";

/// 等待 VS Code 退出的超时（秒）。超时（如未保存对话框长时间未处理）即中止
/// 本次注入，VS Code 保持运行。
const QUIT_WAIT_TIMEOUT_SECS: u64 = 60;

/// 登录 shell 环境解析超时（秒）；VS Code 自身的解析上限同为 10s。
const SHELL_ENV_RESOLVE_TIMEOUT_SECS: u64 = 10;

/// BYOK 注入必须先剥除的继承变量（如 ~/.zshrc 的导出值），再写入本次选中值。
/// 两个 ANTHROPIC_DEFAULT_* 别名也在其中：注入后 BASE_URL 已指向选中网关，
/// 别名若保留 shell 旧值（多指向官方模型），后台任务会拿着网关 base url
/// 请求官方模型 id，破坏 BYOK 意图——因此别名随本次选中值整体重置。
const BYOK_OVERRIDE_KEYS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_MODEL",
    "ANTHROPIC_SMALL_FAST_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
];

/// VS Code 是否已安装（供菜单禁用决策）。
pub fn is_installed() -> bool {
    resolve_vscode_binary().is_ok()
}

/// 带 BYOK env 注入启动 VS Code（GUI detach）。VS Code 正在运行时先请求重启。
pub fn launch(selection: &Selection, apikey: &str) -> Result<()> {
    let model = selection.model.as_ref().context("VS Code 未选中模型")?;

    restart_running_instance_if_any(&selection.provider.name, &model.id)?;

    let mut env = resolve_login_shell_env();
    apply_byok_env(selection, apikey, &mut env);
    // VSCODE_CLI=1：复刻 `code` CLI 的启动语义，令 VS Code 跳过自身 shell env
    // 解析，进程 env 直达扩展宿主与集成终端。
    env.insert("VSCODE_CLI".into(), "1".into());

    let binary = resolve_vscode_binary()?;
    spawn_detached(&binary, &env)?;

    println!();
    println!(
        "启动 VS Code | Provider: {} | Model: {}（BYOK env 注入，VSCODE_CLI=1）",
        selection.provider.name, model.id
    );
    println!();
    Ok(())
}

/// 以普通方式启动 VS Code：等效 Dock 正常打开（已运行时仅激活窗口）。
pub fn launch_plain() -> Result<()> {
    let binary = resolve_vscode_binary()?;
    let app = app_bundle_path(&binary).context("无法从二进制路径定位 VS Code .app bundle")?;
    let status = Command::new("open")
        .arg("-a")
        .arg(&app)
        .status()
        .with_context(|| format!("打开 VS Code 失败: {}", app.display()))?;
    if !status.success() {
        bail!("打开 VS Code 失败（open 退出码 {:?}）", status.code());
    }
    Ok(())
}

/// VS Code 正在运行时：确认 → 优雅退出 → 等待退出。用户拒绝或超时则中止注入。
fn restart_running_instance_if_any(provider_name: &str, model_id: &str) -> Result<()> {
    let binary = resolve_vscode_binary()?;
    // bundle id 解析一次，供检测与退出复用——检测范围与 quit 范围严格一致
    //（同 bundle 同实例），不会把 Insiders 等其他 VS Code 变体误判进来。
    let bundle_id = app_bundle_path(&binary)
        .and_then(|app| plist_value(&app.join("Contents/Info.plist"), "CFBundleIdentifier").ok());
    if !is_app_running(&binary, bundle_id.as_deref()) {
        return Ok(());
    }
    if !confirm_restart(provider_name, model_id)? {
        bail!("用户取消了 VS Code 重启");
    }
    quit_app(&binary)?;
    wait_for_exit(&binary, bundle_id.as_deref())?;
    Ok(())
}

// ── env 构造 ──────────────────────────────────────────────

/// 登录 shell 环境 + 当前进程 env 兜底。shell 解析失败/超时时仅记警告，
/// 注入值本身仍然有效（PATH 等可能不全）。
fn resolve_login_shell_env() -> BTreeMap<String, String> {
    let mut env: BTreeMap<String, String> = std::env::vars().collect();
    match runtime().block_on(dump_login_shell_env()) {
        Ok(dumped) => env.extend(dumped),
        Err(e) => {
            eprintln!("[cx] 警告: 解析登录 shell 环境失败（{e}），仅使用当前进程环境");
        }
    }
    env
}

/// spawn `$SHELL -i -l -c`，marker 包裹 base64(env -0) 块（交互式 shell 可能向
/// stdout 打印任意噪音，只认 marker 之间的内容；思路同 VS Code shellEnv.ts）。
async fn dump_login_shell_env() -> Result<BTreeMap<String, String>> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let mark = random_mark();
    let script = format!("printf '%s' '{mark}'; env -0 | base64; printf '%s' '{mark}'");
    let child = tokio::process::Command::new(&shell)
        .args(["-i", "-l", "-c", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("启动登录 shell `{shell}` 失败"))?;
    let output = tokio::time::timeout(
        Duration::from_secs(SHELL_ENV_RESOLVE_TIMEOUT_SECS),
        child.wait_with_output(),
    )
    .await
    .with_context(|| format!("登录 shell 环境解析超时（{SHELL_ENV_RESOLVE_TIMEOUT_SECS}s）"))?
    .context("等待登录 shell 退出失败")?;
    parse_env_dump(&output.stdout, &mark).context("env dump 中未找到 marker 包裹的 base64 块")
}

/// 从 shell stdout 中提取 marker 之间的 base64 块并还原为 env 映射。
fn parse_env_dump(stdout: &[u8], mark: &str) -> Option<BTreeMap<String, String>> {
    let text = String::from_utf8_lossy(stdout);
    let start = text.find(mark)? + mark.len();
    let rest = &text[start..];
    let end = rest.find(mark)?;
    let b64: String = rest[..end].chars().filter(|c| !c.is_whitespace()).collect();
    let bytes = STANDARD.decode(b64).ok()?;
    let mut env = BTreeMap::new();
    for entry in bytes.split(|b| *b == 0) {
        let entry = String::from_utf8_lossy(entry);
        let Some((key, value)) = entry.split_once('=') else {
            continue;
        };
        if !key.is_empty() {
            env.insert(key.to_string(), value.to_string());
        }
    }
    Some(env)
}

/// 在给定 env 上叠加 Claude Code BYOK env（最高优先级）：先剥除旧 ANTHROPIC_*，
/// 写入选中 provider/model 的值，最后合并 provider/model 自定义 env（配置优先）。
fn apply_byok_env(selection: &Selection, apikey: &str, env: &mut BTreeMap<String, String>) {
    let Some(model) = selection.model.as_ref() else {
        return;
    };
    for key in BYOK_OVERRIDE_KEYS {
        env.remove(*key);
    }
    // 剥除上下文后缀：provider 接收 base id（如 glm-5.2），上下文窗口改经
    // CLAUDE_CODE_MAX_CONTEXT_TOKENS 传达。
    let (api_model_id, ctx_hint) = parse_model_context_suffix(&model.id);
    env.insert("ANTHROPIC_BASE_URL".into(), model.endpoint_url.clone());
    env.insert("ANTHROPIC_API_KEY".into(), apikey.to_string());
    env.insert("ANTHROPIC_MODEL".into(), api_model_id.to_string());
    env.insert("CX_MODEL".into(), api_model_id.to_string());
    // Claude Code 对未识别模型名按内置窗口假设上下文；经 ANTHROPIC_BASE_URL
    // 路由第三方模型时需显式声明，否则 [1m] 模型会被按默认窗口截断。
    if let Some(tokens) = ctx_hint {
        // 选中模型带上下文后缀时窗口声明以本次注入为准，剥除 shell 残留旧值
        //（无后缀模型不设置该变量，保留 shell 原值）。
        env.remove("CLAUDE_CODE_MAX_CONTEXT_TOKENS");
        env.insert("CLAUDE_CODE_MAX_CONTEXT_TOKENS".into(), tokens.to_string());
    }
    // 别名一律重置为选中模型（shell 旧值已在 BYOK_OVERRIDE_KEYS 剥除）：
    // 否则后台任务（haiku 别名）等会尝试请求网关上不存在的官方模型。
    // ANTHROPIC_SMALL_FAST_MODEL 已废弃（由 ANTHROPIC_DEFAULT_HAIKU_MODEL
    // 取代），仅剥除残留值、不再写回。
    env.insert(
        "ANTHROPIC_DEFAULT_SONNET_MODEL".into(),
        api_model_id.to_string(),
    );
    env.insert(
        "ANTHROPIC_DEFAULT_HAIKU_MODEL".into(),
        api_model_id.to_string(),
    );
    // Provider + Model 级自定义环境变量（ResolvedModel.env 已合并 provider +
    // model env，model 优先），覆盖以上默认注入。
    for (key, value) in &model.env {
        env.insert(key.clone(), value.clone());
    }
}

fn random_mark() -> String {
    let mut raw = [0u8; 9];
    rand::thread_rng().fill_bytes(&mut raw);
    URL_SAFE_NO_PAD.encode(raw)
}

// ── App 解析与进程编排 ──────────────────────────────────────

fn resolve_vscode_binary() -> Result<PathBuf> {
    let app_or_bin =
        std::env::var("CX_VSCODE_APP").unwrap_or_else(|_| DEFAULT_APP_PATH.to_string());
    let path = Path::new(&app_or_bin);

    // 若直接指向 MacOS 内二进制，直接用
    if path.is_file() {
        return Ok(path.to_path_buf());
    }

    // 否则视为 .app bundle，解析 CFBundleExecutable
    let info_plist = path.join("Contents/Info.plist");
    if !info_plist.exists() {
        bail!(
            "未找到 VS Code（{app_or_bin}）；可用 CX_VSCODE_APP 环境变量指定 .app 路径或内部二进制"
        );
    }
    let exec_name = plist_value(&info_plist, "CFBundleExecutable")?;
    Ok(path.join("Contents/MacOS").join(exec_name))
}

fn plist_value(info_plist: &Path, key: &str) -> Result<String> {
    let output = Command::new("/usr/libexec/PlistBuddy")
        .args([
            "-c",
            &format!("Print :{key}"),
            &info_plist.to_string_lossy(),
        ])
        .output()
        .with_context(|| format!("读取 {} 的 {key} 失败", info_plist.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("解析 {key} 失败: {}", stderr.trim());
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if value.is_empty() {
        bail!("{key} 为空");
    }
    Ok(value)
}

fn app_bundle_path(binary: &Path) -> Option<PathBuf> {
    binary
        .ancestors()
        .find(|p| p.extension().is_some_and(|ext| ext == "app"))
        .map(Path::to_path_buf)
}

/// 检测 VS Code 是否在运行：优先按 bundle id 统计实例数（System Events），
/// 失败回落 pgrep 二进制路径匹配。
fn is_app_running(binary: &Path, bundle_id: Option<&str>) -> bool {
    if let Some(count) = bundle_id.and_then(count_running_instances) {
        return count > 0;
    }
    Command::new("pgrep")
        .args(["-f", &binary.to_string_lossy()])
        .stdout(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// System Events 进程列表中统计指定 bundle id 的实例数；失败返回 None
///（调用方回落 pgrep）。
fn count_running_instances(bundle_id: &str) -> Option<usize> {
    let output = Command::new("osascript")
        .arg("-e")
        .arg("tell application \"System Events\" to get bundle identifier of every process")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let list = String::from_utf8_lossy(&output.stdout);
    Some(parse_instance_count(&list, bundle_id))
}

/// 解析 System Events 进程列表（逗号分隔，含 `missing value` 项），统计
/// 指定 bundle id 的出现次数。
fn parse_instance_count(list_output: &str, bundle_id: &str) -> usize {
    list_output
        .split(',')
        .filter(|item| item.trim() == bundle_id)
        .count()
}

fn confirm_restart(provider_name: &str, model_id: &str) -> Result<bool> {
    let script = format!(
        "display alert \"Manox 请求重启 Visual Studio Code\" \
         message \"为注入 BYOK 配置（{} · {}），需要退出并重启 VS Code。未保存的更改按 VS Code 退出流程处理（询问或自动恢复）。\" \
         buttons {{\"取消\", \"重启\"}} default button \"重启\" cancel button \"取消\"",
        escape_applescript(provider_name),
        escape_applescript(model_id)
    );
    let output = Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .output()
        .context("osascript 弹出重启确认框失败")?;
    if !output.status.success() {
        // cancel button → 非零退出码
        return Ok(false);
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim() == "重启")
}

fn quit_app(binary: &Path) -> Result<()> {
    let bundle = app_bundle_path(binary).context("无法从二进制路径定位 .app bundle")?;
    let bundle_id = plist_value(&bundle.join("Contents/Info.plist"), "CFBundleIdentifier")?;
    let script = format!(
        "tell application id \"{}\" to quit",
        escape_applescript(&bundle_id)
    );
    let output = Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .output()
        .context("osascript 退出 VS Code 失败")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("退出 VS Code 失败: {}", stderr.trim());
    }
    Ok(())
}

fn wait_for_exit(binary: &Path, bundle_id: Option<&str>) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(QUIT_WAIT_TIMEOUT_SECS);
    while Instant::now() < deadline {
        if !is_app_running(binary, bundle_id) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    bail!(
        "等待 VS Code 退出超时（{QUIT_WAIT_TIMEOUT_SECS}s；可能有未保存更改的退出对话框未处理），已取消注入"
    )
}

fn spawn_detached(binary: &Path, env: &BTreeMap<String, String>) -> Result<()> {
    let mut command = Command::new(binary);
    // env 全量显式传入（登录 shell 解析结果 + BYOK 叠加），不继承 manox/cx 自身环境
    command.env_clear();
    for (key, value) in env {
        command.env(key, value);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0); // 独立进程组：VS Code 脱离 cx/manox 生命周期
    }
    command
        .spawn()
        .with_context(|| format!("启动 VS Code 二进制失败: {}", binary.display()))?;
    Ok(())
}

fn escape_applescript(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ResolvedModel;
    use crate::ResolvedProvider;
    use cx_providers::{CopilotAuth, WireApi};

    fn vscode_test_selection(model_id: &str, env: BTreeMap<String, String>) -> Selection {
        Selection {
            agent_id: "VS Code".into(),
            agent_binary: "claude".into(),
            agent_args: vec!["vscode".into()],
            agent_env: BTreeMap::new(),
            selected_wire_api: WireApi::Anthropic,
            provider: ResolvedProvider {
                name: "百炼".into(),
                has_endpoints: true,
                apikey_source: None,
                env: BTreeMap::new(),
            },
            model: Some(ResolvedModel {
                id: model_id.into(),
                desc: String::new(),
                wire_api: WireApi::Anthropic,
                model_wire_apis: vec![WireApi::Anthropic],
                provider_name: "百炼".into(),
                endpoint_url: "https://dashscope.aliyuncs.com/apps/anthropic".into(),
                visible_agents: vec!["claude".into(), "VS Code".into()],
                copilot_auth: CopilotAuth::ApiKey,
                env,
                apikey_source: None,
                max_tokens: None,
                context: None,
                supports_tools: true,
                supports_images: false,
            }),
            injected_models: Vec::new(),
        }
    }

    #[test]
    fn parse_env_dump_extracts_marker_block() {
        let bytes = b"PATH=/usr/bin\0HOME=/Users/x\0";
        let b64 = STANDARD.encode(bytes);
        let stdout = format!("login noise\nMARK123{b64}MARK123trailing");
        let env = parse_env_dump(stdout.as_bytes(), "MARK123").expect("parse");
        assert_eq!(env.get("PATH").map(String::as_str), Some("/usr/bin"));
        assert_eq!(env.get("HOME").map(String::as_str), Some("/Users/x"));
    }

    #[test]
    fn parse_env_dump_rejects_missing_marker() {
        assert!(parse_env_dump(b"no markers here", "MARK123").is_none());
    }

    /// 模拟 ~/.zshrc 已导出 ANTHROPIC_* 的 shell env：叠加后选中值胜出，
    /// 无冲突键（PATH 等）原样保留。
    #[test]
    fn apply_byok_env_overrides_zshrc_exports() {
        let selection = vscode_test_selection("glm-5.2[1m]", BTreeMap::new());
        let mut env = BTreeMap::new();
        env.insert("PATH".into(), "/opt/homebrew/bin".into());
        env.insert(
            "ANTHROPIC_BASE_URL".into(),
            "https://zshrc.example/anthropic".into(),
        );
        env.insert("ANTHROPIC_API_KEY".into(), "zshrc-key".into());
        env.insert("ANTHROPIC_MODEL".into(), "zshrc-model".into());

        apply_byok_env(&selection, "selected-key", &mut env);

        assert_eq!(
            env.get("PATH").map(String::as_str),
            Some("/opt/homebrew/bin")
        );
        assert_eq!(
            env.get("ANTHROPIC_BASE_URL").map(String::as_str),
            Some("https://dashscope.aliyuncs.com/apps/anthropic")
        );
        assert_eq!(
            env.get("ANTHROPIC_API_KEY").map(String::as_str),
            Some("selected-key")
        );
        assert_eq!(
            env.get("ANTHROPIC_MODEL").map(String::as_str),
            Some("glm-5.2")
        );
        assert_eq!(env.get("CX_MODEL").map(String::as_str), Some("glm-5.2"));
        // [1m] 后缀 → 上下文窗口声明
        assert_eq!(
            env.get("CLAUDE_CODE_MAX_CONTEXT_TOKENS")
                .map(String::as_str),
            Some("1000000")
        );
        // 别名 env 一律重置为选中模型（含 shell 已有旧值）；废弃变量仅剥除
        assert_eq!(
            env.get("ANTHROPIC_DEFAULT_SONNET_MODEL")
                .map(String::as_str),
            Some("glm-5.2")
        );
        assert_eq!(
            env.get("ANTHROPIC_DEFAULT_HAIKU_MODEL").map(String::as_str),
            Some("glm-5.2")
        );
        assert!(!env.contains_key("ANTHROPIC_SMALL_FAST_MODEL"));
    }

    /// provider/model 级 env（ResolvedModel.env）优先于自动注入的默认值。
    #[test]
    fn apply_byok_env_model_env_wins() {
        let mut model_env = BTreeMap::new();
        model_env.insert("ANTHROPIC_DEFAULT_HAIKU_MODEL".into(), "qwen-flash".into());
        model_env.insert("CLAUDE_CODE_MAX_CONTEXT_TOKENS".into(), "131072".into());
        let selection = vscode_test_selection("qwen3.7-max[1m]", model_env);
        let mut env = BTreeMap::new();

        apply_byok_env(&selection, "k", &mut env);

        assert_eq!(
            env.get("ANTHROPIC_DEFAULT_HAIKU_MODEL").map(String::as_str),
            Some("qwen-flash")
        );
        assert_eq!(
            env.get("CLAUDE_CODE_MAX_CONTEXT_TOKENS")
                .map(String::as_str),
            Some("131072")
        );
        // 未覆盖的别名仍自动补齐
        assert_eq!(
            env.get("ANTHROPIC_DEFAULT_SONNET_MODEL")
                .map(String::as_str),
            Some("qwen3.7-max")
        );
    }

    #[test]
    fn parse_instance_count_counts_only_exact_bundle_matches() {
        let list = "com.apple.finder, com.microsoft.VSCode, missing value, \
                    com.microsoft.VSCodeInsiders, com.microsoft.VSCode";
        assert_eq!(parse_instance_count(list, "com.microsoft.VSCode"), 2);
        assert_eq!(
            parse_instance_count(list, "com.microsoft.VSCodeInsiders"),
            1
        );
        assert_eq!(parse_instance_count("", "com.microsoft.VSCode"), 0);
    }

    #[test]
    fn escape_applescript_escapes_quotes_and_backslashes() {
        assert_eq!(escape_applescript("a\"b\\c"), "a\\\"b\\\\c");
    }
}
