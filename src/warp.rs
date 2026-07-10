// Warp 终端 OSC 777 CLI agent 协议集成。
//
// 当 cx 在 Warp 终端中运行时，本模块向 Warp 发送 session_start / stop 事件，
// 使 Warp 的 Agent 侧边栏显示当前 agent 会话状态。
//
// 协议参考：https://yigitkonur.com/reverse-engineering-warp-cli-agent-protocol
//
// OSC 777 格式: \x1b]777;notify;warp://cli-agent;<JSON>\x07
// JSON 信封字段: v, agent, event, session_id, cwd, project（+ 可选扩展字段）

use serde::Serialize;
use std::cell::Cell;
use std::env;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

/// 检查当前终端是否支持 Warp CLI agent 协议。
fn is_warp_terminal() -> bool {
    env::var("WARP_CLI_AGENT_PROTOCOL_VERSION").is_ok()
}

/// OSC 777 事件信封。
#[derive(Serialize)]
struct AgentEvent {
    /// 协议版本，固定为 1。
    v: u8,
    /// Agent 标识符（如 "claude"、"codex"、"copilot"）。
    agent: String,
    /// 事件类型（"session_start"、"stop"）。
    event: String,
    /// 会话唯一标识，用于关联同一会话的多个事件。
    session_id: String,
    /// 当前工作目录（绝对路径）。
    cwd: String,
    /// 项目名（工作目录的基名）。
    project: String,
    /// 选中的模型 ID（可选）。
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    /// 进程退出码（仅 stop 事件）。
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
}

/// Warp 会话句柄，持有 session 标识以便发出配对的 start/stop 事件。
///
/// 实现 `Drop`：当 `WarpSession` 被丢弃时（包括 panic 导致的提前退出），
/// 自动发出 `stop` 事件，确保 Warp 侧边栏不会残留 "running" 状态。
pub struct WarpSession {
    agent_id: String,
    session_id: String,
    model: Option<String>,
    stopped: Cell<bool>,
}

impl WarpSession {
    /// 返回 session ID，供传递给子进程环境变量使用。
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// 发出 stop 事件，表示 agent 会话结束。
    ///
    /// 幂等：多次调用只发出一次事件。
    /// `exit_code` 为 agent 进程的退出码；异常退出（信号终止等）时为 `None`。
    pub fn emit_stop(&self, exit_code: Option<i32>) {
        if self.stopped.get() {
            return;
        }
        self.stopped.set(true);
        emit_event(&AgentEvent {
            v: 1,
            agent: self.agent_id.clone(),
            event: "stop".into(),
            session_id: self.session_id.clone(),
            cwd: cwd_string(),
            project: project_name(),
            model: self.model.clone(),
            exit_code,
        });
    }
}

impl Drop for WarpSession {
    fn drop(&mut self) {
        if !self.stopped.get() {
            emit_event(&AgentEvent {
                v: 1,
                agent: self.agent_id.clone(),
                event: "stop".into(),
                session_id: self.session_id.clone(),
                cwd: cwd_string(),
                project: project_name(),
                model: self.model.clone(),
                exit_code: None,
            });
        }
    }
}

/// 如果运行在 Warp 终端中，发出 session_start 事件并返回会话句柄。
///
/// 返回的 `WarpSession` 可用于后续发出 stop 事件。
/// 非 Warp 环境或 `/dev/tty` 不可用时返回 `None`。
pub fn maybe_emit_session_start(agent_id: &str, model: Option<&str>) -> Option<WarpSession> {
    if !is_warp_terminal() {
        return None;
    }

    let session_id = generate_session_id();
    let session = WarpSession {
        agent_id: agent_id.to_string(),
        session_id,
        model: model.map(|s| s.to_string()),
        stopped: Cell::new(false),
    };

    emit_event(&AgentEvent {
        v: 1,
        agent: session.agent_id.clone(),
        event: "session_start".into(),
        session_id: session.session_id.clone(),
        cwd: cwd_string(),
        project: project_name(),
        model: session.model.clone(),
        exit_code: None,
    });

    Some(session)
}

use crate::session::generate_session_id;

/// 获取当前工作目录的字符串表示。
fn cwd_string() -> String {
    env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// 获取当前工作目录的基名作为项目名。
fn project_name() -> String {
    env::current_dir()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_default()
}

/// 将 OSC 777 事件写入 /dev/tty。
///
/// 写入失败时静默忽略，不影响 agent 启动流程。
fn emit_event(event: &AgentEvent) {
    let Ok(json) = serde_json::to_string(event) else {
        return;
    };

    let mut tty = match OpenOptions::new().write(true).open(Path::new("/dev/tty")) {
        Ok(f) => f,
        Err(_) => return,
    };

    let _ = write!(tty, "\x1b]777;notify;warp://cli-agent;{json}\x07");
    let _ = tty.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_is_32_hex_chars() {
        let id = generate_session_id();
        assert_eq!(id.len(), 32);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn session_ids_are_unique() {
        let a = generate_session_id();
        let b = generate_session_id();
        assert_ne!(a, b);
    }

    #[test]
    fn is_warp_returns_false_without_env() {
        // 测试环境中不太可能设置 WARP_CLI_AGENT_PROTOCOL_VERSION，
        // 但为确保测试隔离，显式移除该变量。
        // SAFETY: 此测试在单线程上下文中运行，不影响其他线程的环境变量。
        unsafe { env::remove_var("WARP_CLI_AGENT_PROTOCOL_VERSION") };
        assert!(!is_warp_terminal());
    }

    #[test]
    fn agent_event_serializes_required_fields() {
        let event = AgentEvent {
            v: 1,
            agent: "claude".into(),
            event: "session_start".into(),
            session_id: "abc123".into(),
            cwd: "/home/user/project".into(),
            project: "project".into(),
            model: Some("opus-4.7".into()),
            exit_code: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""v":1"#));
        assert!(json.contains(r#""agent":"claude""#));
        assert!(json.contains(r#""event":"session_start""#));
        assert!(json.contains(r#""session_id":"abc123""#));
        assert!(json.contains(r#""model":"opus-4.7""#));
        // exit_code 为 None，不应出现在 JSON 中
        assert!(!json.contains("exit_code"));
    }

    #[test]
    fn agent_event_omits_none_optional_fields() {
        let event = AgentEvent {
            v: 1,
            agent: "codex".into(),
            event: "stop".into(),
            session_id: "def456".into(),
            cwd: "/tmp".into(),
            project: "tmp".into(),
            model: None,
            exit_code: Some(0),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""exit_code":0"#));
        // model 为 None，不应出现在 JSON 中
        assert!(!json.contains("model"));
    }

    #[test]
    fn maybe_emit_returns_none_outside_warp() {
        // SAFETY: 此测试在单线程上下文中运行，不影响其他线程的环境变量。
        unsafe { env::remove_var("WARP_CLI_AGENT_PROTOCOL_VERSION") };
        assert!(maybe_emit_session_start("claude", Some("opus-4.7")).is_none());
    }
}
