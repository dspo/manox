// Out-of-process message injection into a running cx session.
//
// `cx send` and library callers share this single path: resolve a session by
// selector, connect its IPC socket, and write a `{"text": ...}` line that the
// relay turns into agent input. `--clear-buffer` (Ctrl+U, 0x15) is composed
// here so the CLI and the library cannot diverge on the framing.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};

use crate::Agent;
use crate::session::{self, SessionRegistry};

/// Byte prepended to clear an agent's input area before injecting (Ctrl+U).
/// Most TUI line editors (claude code included) honor it as "kill to start of
/// line"; the relay forwards it verbatim via the JSON `text` field.
pub const CLEAR_INPUT_BYTE: u8 = 0x15;

/// `CLEAR_INPUT_BYTE` as a `&str`, shared by `compose_effective` (socket path)
/// and `SessionHandle::clear_buffer` / `write_over` (in-process path) so the
/// clear prefix cannot drift between them.
pub(crate) const CLEAR_INPUT: &str = "\u{15}";

/// How a caller picks the target session. The CLI parses its `--session` string
/// into this; library callers construct it directly for type safety and to reach
/// `Agent::CodexPlus` (which the CLI keyword `codex` does not expose).
#[derive(Debug, Clone)]
pub enum SendSelector {
    /// Most recently started live session, any agent.
    Latest,
    /// Most recently started live session of a specific agent.
    ///
    /// `Agent::Codex` matches the codex family (`codex` and `codex+`);
    /// `Agent::CodexPlus` matches only `codex+`.
    Agent(Agent),
    /// Exact cx session id (the 32-hex from `<id>.json`).
    Id(String),
}

/// The session a `send` call landed on — enough to report back, nothing more.
#[derive(Debug, Clone)]
pub struct SendTarget {
    pub id: String,
    pub agent: String,
}

/// Inject a message into a running cx session over its IPC socket — the same
/// path `cx send` takes. `text` is forwarded as `text + '\n'` (a submitted
/// line); with `clear_buffer` the Ctrl+U clear byte is prepended, so an empty
/// `text` clears the input area and a non-empty one overwrites it.
pub fn send(selector: &SendSelector, text: Option<&str>, clear_buffer: bool) -> Result<SendTarget> {
    if text.is_none() && !clear_buffer {
        bail!("请提供 text，或加 --clear-buffer 清空输入区");
    }

    let alive: Vec<SessionRegistry> = session::list_registries()
        .into_iter()
        .filter(|r| Path::new(&r.socket).exists() && session::socket_alive(Path::new(&r.socket)))
        .collect();

    let target = resolve_session(&alive, selector)?;

    let effective = compose_effective(text, clear_buffer);
    let mut stream = UnixStream::connect(&target.socket)
        .with_context(|| format!("连接 session socket 失败: {}", target.socket))?;
    let line = serde_json::to_string(&serde_json::json!({ "text": effective }))
        .context("序列化注入消息失败")?;
    stream
        .write_all(format!("{line}\n").as_bytes())
        .context("写入 session socket 失败")?;
    stream.flush().context("flush session socket 失败")?;

    Ok(SendTarget {
        id: target.id.clone(),
        agent: target.agent.clone(),
    })
}

/// Pick one live session by selector. `started_at` is RFC3339, so a descending
/// lexicographic sort yields the most recent.
pub(crate) fn resolve_session<'a>(
    alive: &'a [SessionRegistry],
    selector: &SendSelector,
) -> Result<&'a SessionRegistry> {
    fn latest_of(mut v: Vec<&SessionRegistry>) -> Result<&SessionRegistry> {
        v.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        v.into_iter()
            .next()
            .ok_or_else(|| anyhow!("没有匹配的活跃 session（用 `cx` 启动一个 agent 后再 send）"))
    }

    match selector {
        SendSelector::Latest => latest_of(alive.iter().collect()),
        SendSelector::Agent(Agent::Claude) => {
            latest_of(alive.iter().filter(|r| r.agent == "claude").collect())
        }
        SendSelector::Agent(Agent::Codex) => latest_of(
            alive
                .iter()
                .filter(|r| matches!(r.agent.as_str(), "codex" | "codex+"))
                .collect(),
        ),
        SendSelector::Agent(Agent::CodexPlus) => {
            latest_of(alive.iter().filter(|r| r.agent == "codex+").collect())
        }
        SendSelector::Agent(Agent::Copilot) => {
            latest_of(alive.iter().filter(|r| r.agent == "copilot").collect())
        }
        SendSelector::Id(id) => alive
            .iter()
            .find(|r| r.id == *id)
            .with_context(|| format!("未找到 session {id}")),
    }
}

/// Compose the `text` payload: prepend the clear byte when requested. The relay
/// appends the trailing `'\n'`, so callers never include it here.
pub(crate) fn compose_effective(text: Option<&str>, clear_buffer: bool) -> String {
    match (text, clear_buffer) {
        (Some(t), true) => format!("{CLEAR_INPUT}{t}"),
        // Ctrl+U then the relay's appended '\n': claude code clears the input box
        // on Ctrl+U and ignores the subsequent empty submit, so the net effect is a
        // cleared box.
        (None, true) => CLEAR_INPUT.to_string(),
        (Some(t), false) => t.to_string(),
        (None, false) => {
            unreachable!("caller must pass text or clear_buffer (validated by `send`)")
        }
    }
}

/// Map a CLI `--session` string to a typed selector. Keywords are
/// case-insensitive; anything else is treated as a literal session id.
pub(crate) fn parse_selector(session: Option<&str>) -> SendSelector {
    let Some(value) = session else {
        return SendSelector::Latest;
    };
    match value.to_ascii_lowercase().as_str() {
        "latest" => SendSelector::Latest,
        "claude" => SendSelector::Agent(Agent::Claude),
        "codex" => SendSelector::Agent(Agent::Codex),
        "copilot" => SendSelector::Agent(Agent::Copilot),
        _ => SendSelector::Id(value.to_ascii_lowercase()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay::transfer::{WriteReq, forward_ipc_stream};
    use std::os::unix::net::UnixStream;
    use std::sync::mpsc;
    use std::time::Duration;

    fn reg(id: &str, agent: &str, started_at: &str) -> SessionRegistry {
        SessionRegistry {
            id: id.into(),
            socket: format!("/tmp/cx-test-{id}.sock"),
            pid: 1,
            agent: agent.into(),
            model: None,
            provider: "test".into(),
            started_at: started_at.into(),
            cwd: "/tmp".into(),
        }
    }

    #[test]
    fn resolve_latest_picks_most_recent() {
        let alive = [
            reg("a", "claude", "2026-07-14T10:00:00Z"),
            reg("b", "codex", "2026-07-14T12:00:00Z"),
        ];
        let got = resolve_session(&alive, &SendSelector::Latest).unwrap();
        assert_eq!(got.id, "b");
    }

    #[test]
    fn resolve_agent_claude_filters_and_picks_recent() {
        let alive = [
            reg("a", "claude", "2026-07-14T10:00:00Z"),
            reg("b", "codex", "2026-07-14T12:00:00Z"),
            reg("c", "claude", "2026-07-14T11:00:00Z"),
        ];
        let got = resolve_session(&alive, &SendSelector::Agent(Agent::Claude)).unwrap();
        assert_eq!(got.id, "c");
    }

    #[test]
    fn resolve_codex_matches_family() {
        let alive = [
            reg("a", "codex", "2026-07-14T10:00:00Z"),
            reg("b", "codex+", "2026-07-14T12:00:00Z"),
            reg("c", "claude", "2026-07-14T13:00:00Z"),
        ];
        let got = resolve_session(&alive, &SendSelector::Agent(Agent::Codex)).unwrap();
        assert_eq!(got.id, "b"); // most recent of codex ∪ codex+
    }

    #[test]
    fn resolve_codexplus_matches_only_plus() {
        let alive = [
            reg("a", "codex", "2026-07-14T12:00:00Z"),
            reg("b", "codex+", "2026-07-14T10:00:00Z"),
        ];
        let got = resolve_session(&alive, &SendSelector::Agent(Agent::CodexPlus)).unwrap();
        assert_eq!(got.id, "b");
    }

    #[test]
    fn resolve_id_hits_exact() {
        let alive = [reg("deadbeef", "claude", "2026-07-14T10:00:00Z")];
        let got = resolve_session(&alive, &SendSelector::Id("deadbeef".into())).unwrap();
        assert_eq!(got.id, "deadbeef");
    }

    #[test]
    fn resolve_id_misses_errors() {
        let alive = [reg("deadbeef", "claude", "2026-07-14T10:00:00Z")];
        let err = resolve_session(&alive, &SendSelector::Id("nope".into())).unwrap_err();
        assert!(format!("{err}").contains("未找到 session nope"));
    }

    #[test]
    fn compose_effective_clear_only_is_ctrl_u() {
        let s = compose_effective(None, true);
        assert_eq!(s.as_bytes(), &[CLEAR_INPUT_BYTE]);
    }

    #[test]
    fn compose_effective_clear_plus_text_prepends_ctrl_u() {
        let s = compose_effective(Some("hi"), true);
        assert_eq!(s.as_bytes(), &[CLEAR_INPUT_BYTE, b'h', b'i']);
    }

    #[test]
    fn compose_effective_text_only_is_verbatim() {
        let s = compose_effective(Some("hi"), false);
        assert_eq!(s.as_bytes(), b"hi");
    }

    /// End-to-end: compose → JSON → relay decode must yield `0x15 0x0a` for a
    /// clear-only injection (clear byte + the relay's appended newline).
    #[test]
    fn clear_only_round_trips_through_relay() {
        let effective = compose_effective(None, true);
        let line = serde_json::to_string(&serde_json::json!({ "text": effective })).unwrap();
        // serde_json escapes the control byte; the relay must decode it back.
        assert!(
            line.contains("\\u0015"),
            "expected 0x15 escaped, got: {line}"
        );

        let (mut a, b) = UnixStream::pair().unwrap();
        let (tx, rx) = mpsc::channel();
        forward_ipc_stream(b, tx);
        a.write_all(format!("{line}\n").as_bytes()).unwrap();
        a.flush().unwrap();

        match rx.recv_timeout(Duration::from_secs(2)).unwrap() {
            WriteReq::Bytes(b) => assert_eq!(b, [CLEAR_INPUT_BYTE, b'\n']),
            _ => panic!("expected Bytes"),
        }
    }

    #[test]
    fn send_rejects_empty_without_clear() {
        let err = send(&SendSelector::Latest, None, false).unwrap_err();
        assert!(format!("{err}").contains("--clear-buffer"));
    }

    #[test]
    fn parse_selector_defaults_to_latest_and_maps_keywords() {
        assert!(matches!(parse_selector(None), SendSelector::Latest));
        assert!(matches!(
            parse_selector(Some("latest")),
            SendSelector::Latest
        ));
        assert!(matches!(
            parse_selector(Some("Claude")),
            SendSelector::Agent(Agent::Claude)
        ));
        assert!(matches!(
            parse_selector(Some("CODEX")),
            SendSelector::Agent(Agent::Codex)
        ));
        // ids are 32-hex lowercase; an uppercase id is lowercased to match.
        match parse_selector(Some("DeadBeef")) {
            SendSelector::Id(s) => assert_eq!(s, "deadbeef"),
            _ => panic!("expected Id"),
        }
    }
}
