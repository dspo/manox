//! ChatGPT.app 启动 + renderer 注入编排。
//!
//! 入口 [`launch_with_injection`]：在 `run_launcher` 中对 `ChatGPT.app` 分流调用。
//!
//! 设计要点：
//! - **直接启动 Electron 二进制**（`ChatGPT.app/Contents/MacOS/<CFBundleExecutable>`）作为子进程，
//!   而非 `open -a`。这样子进程继承 cx 的 env，`CODEX_HOME`/`<env_key>=<apikey>` 直接生效，
//!   无需 `launchctl setenv` 污染全局登录会话（避免密钥泄漏 + 残留）。
//! - **spawn → CDP 注入 → detach**：先 spawn（stdio→null、独立进程组）拿到运行中的进程，
//!   经 CDP 注入模型列表脚本后**立即 detach 返回**——ChatGPT.app 是 GUI 应用（独立窗口），
//!   不应阻塞终端，cx 注入完成即让出终端，App 在后台继续运行。
//! - `model_reasoning_effort` 与注入脚本的默认 effort 共用，保持下拉默认值与后端一致。

pub mod catalog;
pub mod cdp;
pub mod inject;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::ChatGptAppPrepared;
use crate::Selection;
use crate::prepare_chatgpt_launch_home_for_app;
use crate::probe::runtime;
use crate::warp;
use cx_providers::{ChatGptAppSettings, ModelInjection};

const DEFAULT_APP_PATH: &str = "/Applications/ChatGPT.app";
/// 等待 ChatGPT.app renderer 注册到 CDP 的最长时长。
const CDP_READY_TIMEOUT_SECS: u64 = 20;

/// 启动 ChatGPT.app 并注入自定义模型列表到 renderer。`apikey` 由调用方解析后传入：
/// CLI 路径经 `resolve_apikey_interactive`（缺失时可交互补齐），GUI 嵌入路径
/// （`crate::launch_chatgpt_app`）经非交互的 `resolve_apikey`。
pub fn launch_with_injection(
    selection: &Selection,
    apikey: &str,
    _passthrough_args: &[String],
    chatgpt_settings: &ChatGptAppSettings,
) -> Result<()> {
    let provider = &selection.provider;
    let default_model = selection
        .model
        .as_ref()
        .context("ChatGPT.app 未选中默认模型")?;
    let injected = &selection.injected_models;
    if injected.is_empty() {
        bail!(
            "Provider `{}` 下没有支持 Responses wire api 的模型，无法注入 ChatGPT.app",
            provider.name
        );
    }

    // 1. 写 config.toml，拿到 codex_home / env_key / reasoning_effort
    let prepared = prepare_chatgpt_launch_home_for_app(
        default_model,
        provider,
        selection.selected_wire_api,
        &selection.injected_models,
        chatgpt_settings,
    )?;
    let ChatGptAppPrepared {
        codex_home,
        env_key,
        reasoning_effort,
        custom_env,
    } = prepared;

    // 注入模式：List 走完整 CDP 注入；Single 仅 config.toml（官方机制），不开调试端口。
    let inject_list = chatgpt_settings.model_injection == ModelInjection::List;

    // 2. 选取 CDP 端口（在 spawn 前确定，作为启动参数传入）。Single 模式不需要。
    let debug_port = if inject_list {
        Some(cdp::pick_debug_port()?)
    } else {
        None
    };

    // 3. 定位 App 并解析内部可执行二进制路径
    let binary = resolve_codex_binary()?;

    // 4. Warp 集成：在启动前发出 session_start，并把 session ID 传给子进程
    let warp_session = warp::maybe_emit_session_start("ChatGPT.app", Some(&default_model.id));

    // 5. 构造启动命令：直接启动二进制；List 模式附加远程调试端口。env 直接设到子进程
    //    （继承），不污染全局。
    //    GUI app detach 运行：stdio 重定向到 null + 独立进程组，使 cx 退出后 App 仍存活、终端不被占。
    let mut command = Command::new(&binary);
    if let Some(port) = debug_port {
        let origin = format!("http://127.0.0.1:{port}");
        command.args([
            &format!("--remote-debugging-port={port}"),
            &format!("--remote-allow-origins={origin}"),
        ]);
    }
    command
        .env("CODEX_HOME", &codex_home)
        .env(&env_key, apikey)
        .env("CX_MODEL", default_model.api_model_id())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0); // 独立进程组，脱离 cx 的终端会话
    }
    if let Some(ref session) = warp_session {
        command.env("CX_WARP_SESSION_ID", session.session_id());
    }

    // Provider 级 + Model 级自定义环境变量（ResolvedModel.env 已合并 provider + model，model 优先）
    for (k, v) in &default_model.env {
        command.env(k, v);
    }

    // Settings → 外部工具 的自定义环境变量。cx 保留键（CODEX_HOME / CX_MODEL /
    // CX_WARP_SESSION_ID / env_key）忽略并告警，防止设置面板破坏注入机制。
    let reserved = crate::chatgpt_reserved_env_keys(&env_key);
    for (k, v) in &custom_env {
        if reserved.contains(k) {
            println!("[cx] 忽略自定义环境变量 {k}: 保留键");
            continue;
        }
        command.env(k, v);
    }

    // 打印启动摘要
    println!();
    match debug_port {
        Some(port) => println!(
            "启动 ChatGPT.app | Provider: {} | Model: {} | 注入 {} 个模型（CDP 端口 {port}）",
            provider.name,
            default_model.id,
            injected.len()
        ),
        None => println!(
            "启动 ChatGPT.app | Provider: {} | Model: {} | 单模型模式（config.toml 官方机制，无 CDP）",
            provider.name, default_model.id
        ),
    }
    println!();

    // 6. spawn（不等待）；List 模式随后经 CDP 注入脚本。
    let mut child = command
        .spawn()
        .with_context(|| format!("启动 ChatGPT.app 二进制失败: {}", binary.display()))?;

    if inject_list {
        // 7. 等待 CDP 就绪并注入脚本（async，复用 probe 的 tokio runtime）
        let port = debug_port.expect("inject_list 时 debug_port 必为 Some");
        let injected_clone = injected.clone();
        let reasoning_effort_clone = reasoning_effort.clone();
        let inject_result = runtime().block_on(async {
            let ws_url =
                cdp::wait_for_page_target(port, Duration::from_secs(CDP_READY_TIMEOUT_SECS))
                    .await?;
            let script = inject::build_injection_script(&injected_clone, &reasoning_effort_clone);
            cdp::inject_script(&ws_url, &script).await?;
            println!(
                "[cx] 已注入 {} 个模型（默认 {}，effort {}）",
                injected_clone.len(),
                injected_clone.first().map(|m| m.id.as_str()).unwrap_or(""),
                reasoning_effort_clone
            );
            Result::<()>::Ok(())
        });

        // 注入失败时主动杀掉刚启动的子进程，避免留下一个无注入的 ChatGPT.app 残留窗口。
        if let Err(err) = inject_result {
            let _ = child.kill();
            let _ = child.wait();
            return Err(err).context("CDP 注入失败，已终止 ChatGPT.app");
        }
    } else {
        println!("[cx] 单模型模式：跳过 CDP 注入（模型由 config.toml 官方机制提供）");
    }

    // 8. ChatGPT.app 是 GUI 应用（独立窗口），不应阻塞终端。注入完成后 detach：
    //    spawn 时已设独立进程组 + stdio→null，此处不 wait，drop(child) 不会杀子进程，
    //    cx 立即返回让出终端，App 在后台继续运行。
    //    （不同于 claude/codex-cli 那类终端内 agent——它们才需要 spawn+wait+退出摘要。）
    drop(child);
    // Warp：GUI app 与终端生命周期解耦，发了 session_start 即可；不等待退出，立即补发 stop 收尾。
    if let Some(session) = warp_session {
        session.emit_stop(None);
    }
    println!("[cx] ChatGPT.app 已在后台启动，终端已释放。");
    Ok(())
}

/// 解析 ChatGPT.app 内部可执行二进制路径。
///
/// 优先环境变量 `CX_CHATGPT_APP`（指向 `.app` 或内部二进制均可），
/// 其次 `CX_CODEX_APP`（向后兼容），否则默认 `/Applications/ChatGPT.app`。
/// 从 `Info.plist` 的 `CFBundleExecutable` 读取可执行名，拼接 `Contents/MacOS/<name>`。
fn resolve_codex_binary() -> Result<PathBuf> {
    let app_or_bin = std::env::var("CX_CHATGPT_APP")
        .or_else(|_| std::env::var("CX_CODEX_APP"))
        .unwrap_or_else(|_| DEFAULT_APP_PATH.to_string());
    let path = Path::new(&app_or_bin);

    // 若直接指向 MacOS 内二进制，直接用
    if path.is_file() {
        return Ok(path.to_path_buf());
    }

    // 否则视为 .app bundle，解析 CFBundleExecutable
    let info_plist = path.join("Contents/Info.plist");
    if !info_plist.exists() {
        bail!(
            "未找到 ChatGPT.app（{app_or_bin}）；可用 CX_CHATGPT_APP 或 CX_CODEX_APP 环境变量指定 .app 路径或内部二进制"
        );
    }
    let output = Command::new("/usr/libexec/PlistBuddy")
        .args([
            "-c",
            "Print :CFBundleExecutable",
            &info_plist.to_string_lossy(),
        ])
        .output()
        .context("读取 ChatGPT.app CFBundleExecutable 失败")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "解析 ChatGPT.app CFBundleExecutable 失败: {}",
            stderr.trim()
        );
    }
    let exec_name = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if exec_name.is_empty() {
        bail!("ChatGPT.app CFBundleExecutable 为空");
    }
    Ok(path.join("Contents/MacOS").join(exec_name))
}
