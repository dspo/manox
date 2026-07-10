// Per-session identity, registry, and discovery.
//
// cx 为每个启动的 agent 会话生成一个稳定的 cx session id（独立于 Warp 的 session id，
// 始终生成），用于命名 IPC socket 与注册表 JSON，使外部进程能发现并向运行中的会话注入消息。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rand::RngCore;
use serde::{Deserialize, Serialize};

use crate::cx_state_dir;

/// Sessions 子目录：`~/.config/cx/sessions/`，存放每个活跃会话的 `<id>.sock` 与 `<id>.json`。
pub(crate) fn sessions_dir() -> Result<PathBuf> {
    Ok(cx_state_dir()?.join("sessions"))
}

/// `<id>.sock` 的绝对路径——外部进程经此 Unix socket 注入消息。
pub(crate) fn socket_path(id: &str) -> Result<PathBuf> {
    Ok(sessions_dir()?.join(format!("{id}.sock")))
}

/// `<id>.json` 的绝对路径——会话注册表，供 `cx send` 发现与挑选会话。
pub(crate) fn registry_path(id: &str) -> Result<PathBuf> {
    Ok(sessions_dir()?.join(format!("{id}.json")))
}

/// 生成 32 字符随机 hex 作为 cx session ID。
pub(crate) fn generate_session_id() -> String {
    let mut rng = rand::thread_rng();
    let mut bytes = [0u8; 16];
    rng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 活跃会话的注册表条目，序列化为 `<id>.json`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SessionRegistry {
    pub(crate) id: String,
    pub(crate) socket: String,
    pub(crate) pid: u32,
    pub(crate) agent: String,
    pub(crate) model: Option<String>,
    pub(crate) provider: String,
    /// RFC3339 启动时间，用于多会话时挑选最新。
    pub(crate) started_at: String,
    pub(crate) cwd: String,
}

/// 写入注册表条目（原子写）。
pub(crate) fn write_registry(reg: &SessionRegistry) -> Result<()> {
    let path = registry_path(&reg.id)?;
    let content = serde_json::to_string(reg).context("序列化 session 注册表失败")?;
    crate::write_string_atomic(&path, &content)
}

/// 删除指定会话的注册表 JSON 与 socket 文件（退出时清理）。
pub(crate) fn cleanup_session(id: &str) {
    if let Ok(path) = socket_path(id) {
        let _ = std::fs::remove_file(&path);
    }
    if let Ok(path) = registry_path(id) {
        let _ = std::fs::remove_file(&path);
    }
}

/// 扫描注册表目录，返回所有可解析的条目（不校验存活性——由连接探活决定）。
pub(crate) fn list_registries() -> Vec<SessionRegistry> {
    let dir = match sessions_dir() {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(reg) = serde_json::from_str::<SessionRegistry>(&content) {
                out.push(reg);
            }
        }
    }
    out
}

/// 判断 Unix socket 路径上是否有监听者（连得上即存活，ECONNREFUSED 视为僵尸）。
pub(crate) fn socket_alive(path: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(path).is_ok()
}
