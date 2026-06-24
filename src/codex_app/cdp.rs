//! 最小 CDP（Chrome DevTools Protocol）客户端。
//!
//! 仅实现 cx 注入 Codex.app renderer 所需的子集：
//! 1. 选取一个空闲的远程调试端口；
//! 2. 轮询 CDP HTTP 端点，拿到 page target 的 `webSocketDebuggerUrl`；
//! 3. 连接 WebSocket，发送 `Page.addScriptToEvaluateOnNewDocument` 与 `Runtime.evaluate`。
//!
//! 仅实现 cx 注入所需的最小 CDP 命令子集（HTTP 取 target + WS 发命令）。

use anyhow::{Context, Result, anyhow};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::time::{Duration, Instant};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

/// CDP 调试端口探测起点。
const CDP_BASE_PORT: u16 = 9559;

/// 找一个可用的 CDP 调试端口：从 `CDP_BASE_PORT` 起依次尝试 bind，最多 32 个。
pub fn pick_debug_port() -> Result<u16> {
    for offset in 0..32u16 {
        let port = CDP_BASE_PORT + offset;
        // bind 成功即说明端口空闲；bind 返回的 listener 在表达式结束时立即 drop，释放端口供 Codex 绑定。
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Ok(port);
        }
    }
    Err(anyhow!(
        "无可用 CDP 调试端口（{}~{} 均被占用）",
        CDP_BASE_PORT,
        CDP_BASE_PORT + 31
    ))
}

/// 轮询 CDP HTTP 端点，返回第一个 `type == "page"` 的 target 的 `webSocketDebuggerUrl`。
///
/// Codex.app 启动后 renderer 需要一点时间才注册到 CDP；在 `timeout` 内重试。
pub async fn wait_for_page_target(debug_port: u16, timeout: Duration) -> Result<String> {
    let list_url = format!("http://127.0.0.1:{debug_port}/json");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()?;
    let deadline = Instant::now() + timeout;
    let mut got_http = false; // 是否曾成功连到端口（区分「端口没人监听」与「监听了但非 Codex CDP」）
    loop {
        if Instant::now() >= deadline {
            if got_http {
                return Err(anyhow!(
                    "端口 {debug_port} 有进程在监听，但未返回 Codex page target——\
                     该端口可能被其它程序（如 Chrome）占用，而非 Codex.app 的调试端口"
                ));
            }
            return Err(anyhow!(
                "等待 Codex.app CDP 就绪超时（端口 {debug_port} 无响应）：\
                 App 可能未启动、崩溃，或不支持远程调试端口"
            ));
        }
        if let Ok(resp) = client.get(&list_url).send().await {
            got_http = true;
            if let Ok(targets) = resp.json::<Vec<Value>>().await {
                for target in &targets {
                    if target.get("type").and_then(|v| v.as_str()) == Some("page") {
                        if let Some(ws) =
                            target.get("webSocketDebuggerUrl").and_then(|v| v.as_str())
                        {
                            return Ok(ws.to_string());
                        }
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

/// 连接 CDP page target 注入脚本。
///
/// 时序：cx 连上 CDP 时 App React 往往已渲染首屏，故注入脚本内置 MutationObserver + 定时器，
/// 在下拉/子菜单挂载时持续 patch。这里 `addScriptToEvaluateOnNewDocument` 覆盖后续导航，
/// `Runtime.evaluate` 对当前页立即生效。
/// 注：不做 `Page.reload`——实测 reload 后 fiber 尚未就绪，首次 patch 反而落空；
/// 保持当前页注入 + observer 持续补 patch 更稳，trigger 首屏陈旧会在用户首次点开下拉时自愈。
pub async fn inject_script(ws_url: &str, script: &str) -> Result<()> {
    let (mut ws, _) = connect_async(ws_url)
        .await
        .with_context(|| format!("连接 CDP WebSocket 失败: {ws_url}"))?;

    send_command(
        &mut ws,
        1,
        "Page.addScriptToEvaluateOnNewDocument",
        json!({ "source": script }),
    )
    .await?;
    send_command(
        &mut ws,
        2,
        "Runtime.evaluate",
        json!({ "expression": script, "awaitPromise": false }),
    )
    .await?;

    Ok(())
}

/// 发送一条 CDP 命令并等待对应 `id` 的响应（跳过期间的事件消息）。
async fn send_command(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    id: u64,
    method: &str,
    params: Value,
) -> Result<Value> {
    let cmd = json!({ "id": id, "method": method, "params": params });
    ws.send(Message::Text(cmd.to_string()))
        .await
        .context("发送 CDP 命令失败")?;

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if Instant::now() >= deadline {
            return Err(anyhow!("等待 CDP 响应（id={id}, method={method}）超时"));
        }
        let Some(msg) = ws.next().await else {
            return Err(anyhow!("CDP WebSocket 在等待响应时关闭"));
        };
        let text = match msg.context("读取 CDP 消息失败")? {
            Message::Text(t) => t,
            Message::Ping(_)
            | Message::Pong(_)
            | Message::Binary(_)
            | Message::Close(_)
            | Message::Frame(_) => continue,
        };
        let value: Value = serde_json::from_str(&text).context("解析 CDP 响应失败")?;
        if value.get("id").and_then(|v| v.as_u64()) == Some(id) {
            if let Some(err) = value.get("error") {
                return Err(anyhow!("CDP 命令 {method} 返回错误: {err}"));
            }
            return Ok(value);
        }
        // 否则是事件消息，继续等待。
    }
}
