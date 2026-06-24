//! Codex.app 启动 + renderer 注入编排。
//!
//! 入口 [`launch_with_injection`]：在 `run_launcher` 中对 `Codex.app` 分流调用。
//!
//! 设计要点：
//! - **直接启动 Electron 二进制**（`Codex.app/Contents/MacOS/<CFBundleExecutable>`）作为子进程，
//!   而非 `open -a`。这样子进程继承 cx 的 env，`CODEX_HOME`/`<env_key>=<apikey>` 直接生效，
//!   无需 `launchctl setenv` 污染全局登录会话（避免密钥泄漏 + 残留）。
//! - **spawn → CDP 注入 → detach**：先 spawn（stdio→null、独立进程组）拿到运行中的进程，
//!   经 CDP 注入模型列表脚本后**立即 detach 返回**——Codex.app 是 GUI 应用（独立窗口），
//!   不应阻塞终端，cx 注入完成即让出终端，App 在后台继续运行。
//! - `model_reasoning_effort` 与注入脚本的默认 effort 共用，保持下拉默认值与后端一致。

pub mod cdp;
pub mod inject;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::CodexAppPrepared;
use crate::Selection;
use crate::prepare_codex_launch_home_for_app;
use crate::probe::runtime;
use crate::resolve_apikey_interactive;
use crate::warp;

const DEFAULT_APP_PATH: &str = "/Applications/Codex.app";
/// 等待 Codex.app renderer 注册到 CDP 的最长时长。
const CDP_READY_TIMEOUT_SECS: u64 = 20;

/// 启动 Codex.app 并注入自定义模型列表到 renderer。
pub fn launch_with_injection(selection: &Selection, _passthrough_args: &[String]) -> Result<()> {
    let provider = &selection.provider;
    let default_model = selection
        .model
        .as_ref()
        .context("Codex.app 未选中默认模型")?;
    let injected = &selection.injected_models;
    if injected.is_empty() {
        bail!(
            "Provider `{}` 下没有支持 Responses wire api 的模型，无法注入 Codex.app",
            provider.name
        );
    }

    // 1. 解析 API Key
    let apikey = if let Some(ref source) = provider.apikey_source {
        resolve_apikey_interactive(source)?
    } else {
        bail!(
            "Provider `{}` 需要 API Key 但未配置 apikey_source",
            provider.name
        );
    };
    if apikey.is_empty() {
        bail!("Codex.app 需要 API Key，但未提供");
    }

    // 2. 写 config.toml，拿到 codex_home / env_key / reasoning_effort
    let prepared =
        prepare_codex_launch_home_for_app(default_model, provider, selection.selected_wire_api)?;
    let CodexAppPrepared {
        codex_home,
        env_key,
        reasoning_effort,
    } = prepared;

    // 3. 选取 CDP 端口（在 spawn 前确定，作为启动参数传入）
    let debug_port = cdp::pick_debug_port()?;

    // 4. 定位 App 并解析内部可执行二进制路径
    let binary = resolve_codex_binary()?;

    // 5. Warp 集成：在启动前发出 session_start，并把 session ID 传给子进程
    let warp_session = warp::maybe_emit_session_start("Codex.app", Some(&default_model.id));

    // 6. 构造启动命令：直接启动二进制 + 远程调试端口。env 直接设到子进程（继承），不污染全局。
    //    GUI app detach 运行：stdio 重定向到 null + 独立进程组，使 cx 退出后 App 仍存活、终端不被占。
    let origin = format!("http://127.0.0.1:{debug_port}");
    let mut command = Command::new(&binary);
    command
        .args([
            &format!("--remote-debugging-port={debug_port}"),
            &format!("--remote-allow-origins={origin}"),
        ])
        .env("CODEX_HOME", &codex_home)
        .env(&env_key, &apikey)
        .env("CX_MODEL", &default_model.id)
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

    // 打印启动摘要
    println!();
    println!(
        "启动 Codex.app | Provider: {} | Model: {} | 注入 {} 个模型（CDP 端口 {debug_port}）",
        provider.name,
        default_model.id,
        injected.len()
    );
    println!();

    // 7. spawn（不等待），随后经 CDP 注入脚本
    let mut child = command
        .spawn()
        .with_context(|| format!("启动 Codex.app 二进制失败: {}", binary.display()))?;

    // 8. 等待 CDP 就绪并注入脚本（async，复用 probe 的 tokio runtime）
    let injected_clone = injected.clone();
    let reasoning_effort_clone = reasoning_effort.clone();
    let inject_result = runtime().block_on(async {
        let ws_url =
            cdp::wait_for_page_target(debug_port, Duration::from_secs(CDP_READY_TIMEOUT_SECS))
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

    // 注入失败时主动杀掉刚启动的子进程，避免留下一个无注入的 Codex.app 残留窗口。
    if let Err(err) = inject_result {
        let _ = child.kill();
        let _ = child.wait();
        return Err(err).context("CDP 注入失败，已终止 Codex.app");
    }

    // 9. Codex.app 是 GUI 应用（独立窗口），不应阻塞终端。注入完成后 detach：
    //    spawn 时已设独立进程组 + stdio→null，此处不 wait，drop(child) 不会杀子进程，
    //    cx 立即返回让出终端，App 在后台继续运行。
    //    （不同于 claude/codex-cli 那类终端内 agent——它们才需要 spawn+wait+退出摘要。）
    drop(child);
    // Warp：GUI app 与终端生命周期解耦，发了 session_start 即可；不等待退出，立即补发 stop 收尾。
    if let Some(session) = warp_session {
        session.emit_stop(None);
    }
    println!("[cx] Codex.app 已在后台启动，终端已释放。");
    Ok(())
}

/// 解析 Codex.app 内部可执行二进制路径。
///
/// 优先环境变量 `CX_CODEX_APP`（指向 `.app` 或内部二进制均可），否则默认 `/Applications/Codex.app`。
/// 从 `Info.plist` 的 `CFBundleExecutable` 读取可执行名，拼接 `Contents/MacOS/<name>`。
fn resolve_codex_binary() -> Result<PathBuf> {
    let app_or_bin = std::env::var("CX_CODEX_APP").unwrap_or_else(|_| DEFAULT_APP_PATH.to_string());
    let path = Path::new(&app_or_bin);

    // 若直接指向 MacOS 内二进制，直接用
    if path.is_file() {
        return Ok(path.to_path_buf());
    }

    // 否则视为 .app bundle，解析 CFBundleExecutable
    let info_plist = path.join("Contents/Info.plist");
    if !info_plist.exists() {
        bail!(
            "未找到 Codex.app（{app_or_bin}）；可用 CX_CODEX_APP 环境变量指定 .app 路径或内部二进制"
        );
    }
    let output = Command::new("/usr/libexec/PlistBuddy")
        .args([
            "-c",
            "Print :CFBundleExecutable",
            &info_plist.to_string_lossy(),
        ])
        .output()
        .context("读取 Codex.app CFBundleExecutable 失败")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("解析 Codex.app CFBundleExecutable 失败: {}", stderr.trim());
    }
    let exec_name = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if exec_name.is_empty() {
        bail!("Codex.app CFBundleExecutable 为空");
    }
    Ok(path.join("Contents/MacOS").join(exec_name))
}
