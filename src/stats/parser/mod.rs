//! 各 agent 的日志解析。
//!
//! 解析阶段产出"未去重"的 [`RawEntry`]，由上层 `mod.rs` 在所有文件解析完之后做
//! 跨文件去重再聚合到 [`UsageRecord`]。
//!
//! 设计参考 ccusage `rust/crates/ccusage/src/adapter/{claude,codex,copilot}/`。

pub(super) mod claude;
pub(super) mod codex;
pub(super) mod copilot;
pub(super) mod mimo;
pub(super) mod omp_session;
pub(super) mod pi;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use super::date::{date_field, parse_ymd};

/// 一个未去重的"用量观测"条目。各 agent 的解析器都把日志归一化成它。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct RawEntry {
    pub(super) agent: String,
    pub(super) model: String,
    /// `YYYY-MM-DD`
    pub(super) date: String,
    pub(super) input_tokens: u64,
    pub(super) output_tokens: u64,
    pub(super) cache_read_input_tokens: u64,
    pub(super) cache_creation_input_tokens: u64,
    /// reasoning_output_tokens 已包含在 `output_tokens` 中（codex/copilot），
    /// 这里冗余保存只供未来展示，不参与汇总。
    #[serde(default)]
    pub(super) reasoning_output_tokens: u64,
    /// 用于跨文件去重的稳定主键（同一 agent 内唯一）。
    /// - claude/omp/mimo: None（raw-sum 口径，不按 message id 去重）
    /// - codex/zed: `session_id|timestamp_iso`
    /// - copilot: 见 copilot 内部说明
    pub(super) dedup_primary: Option<String>,
    /// 次级主键（与 primary 联合一起去重）。
    /// - claude/omp/mimo: None
    /// - codex 系列: 一组 token 数，让重复 snapshot 落到同一 key
    pub(super) dedup_secondary: Option<String>,
    /// 是否为 sidechain 记录（仅 claude 使用）。
    #[serde(default)]
    pub(super) is_sidechain: bool,
    /// 会话标识（可选，用于明细展示和溯源）。
    #[serde(default)]
    pub(super) session_id: Option<String>,
    /// 消息标识（可选，来自源数据中的自然 ID）。
    #[serde(default)]
    pub(super) message_id: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub(super) enum SourceKind {
    Claude,
    /// codex / zed 共用一套日志格式。
    CodexLike(&'static str),
    /// github copilot OpenTelemetry 导出。
    Copilot(&'static str),
    /// OMP session jsonl（~/.omp/agent/sessions/）。
    OmpSession,
    /// Mimo CLI session SQLite（~/.local/share/mimocode/mimocode.db）。
    MimoSession,
    /// pi coding-agent session jsonl（~/.pi/agent/sessions/）。
    PiSession,
}

pub(super) struct ParseResult {
    pub(super) entries: Vec<RawEntry>,
    /// 可以安全跳过的字节位置；若尾部存在不完整 JSON，则停在最后一个完整记录之后。
    pub(super) consumed_bytes: u64,
}

impl SourceKind {
    pub(super) fn supports_append_scan(self) -> bool {
        matches!(
            self,
            SourceKind::Claude | SourceKind::OmpSession | SourceKind::PiSession
        )
    }
}

pub(super) fn parse_file(path: &Path, kind: SourceKind) -> Result<ParseResult> {
    match kind {
        SourceKind::MimoSession => mimo::parse(path)
            .map(|entries| ParseResult {
                entries,
                consumed_bytes: 0,
            })
            .with_context(|| format!("Mimo 解析失败 ({})", path.display())),
        _ => {
            let bytes = std::fs::read(path)
                .with_context(|| format!("读取日志失败 ({})", path.display()))?;
            Ok(parse_jsonl_bytes(path, kind, &bytes))
        }
    }
}

pub(super) fn parse_file_from_offset(
    path: &Path,
    kind: SourceKind,
    offset: u64,
) -> Result<ParseResult> {
    match kind {
        SourceKind::MimoSession => parse_file(path, kind),
        _ => {
            let mut file =
                File::open(path).with_context(|| format!("打开日志失败 ({})", path.display()))?;
            file.seek(SeekFrom::Start(offset))
                .with_context(|| format!("定位日志失败 ({})", path.display()))?;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)
                .with_context(|| format!("读取日志尾部失败 ({})", path.display()))?;
            Ok(parse_jsonl_bytes(path, kind, &bytes))
        }
    }
}

fn parse_jsonl_bytes(path: &Path, kind: SourceKind, bytes: &[u8]) -> ParseResult {
    let content = String::from_utf8_lossy(bytes);
    ParseResult {
        entries: parse_jsonl_content(path, kind, &content),
        consumed_bytes: consumed_jsonl_bytes(bytes),
    }
}

fn parse_jsonl_content(path: &Path, kind: SourceKind, content: &str) -> Vec<RawEntry> {
    match kind {
        SourceKind::Claude => claude::parse(content),
        SourceKind::CodexLike(agent) => {
            let fallback_date = fallback_date_from_path(path);
            codex::parse(content, agent, fallback_date.as_deref(), path)
        }
        SourceKind::Copilot(agent) => copilot::parse(content, agent, path),
        SourceKind::OmpSession => omp_session::parse(content),
        SourceKind::PiSession => pi::parse(content),
        SourceKind::MimoSession => unreachable!(),
    }
}

fn consumed_jsonl_bytes(bytes: &[u8]) -> u64 {
    if bytes.is_empty() {
        return 0;
    }
    if bytes.last() == Some(&b'\n') {
        return bytes.len() as u64;
    }

    let last_newline = bytes.iter().rposition(|b| *b == b'\n');
    let tail_start = last_newline.map_or(0, |idx| idx + 1);
    let tail = &bytes[tail_start..];
    if tail.is_empty() {
        return tail_start as u64;
    }

    let is_complete_json = std::str::from_utf8(tail)
        .ok()
        .map(|line| line.trim_end_matches('\r'))
        .filter(|line| !line.is_empty())
        .is_some_and(|line| serde_json::from_str::<Value>(line).is_ok());

    if is_complete_json {
        bytes.len() as u64
    } else {
        tail_start as u64
    }
}

pub(super) fn u64_field(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

pub(super) fn fallback_date_from_path(path: &Path) -> Option<String> {
    let mut cursor = path.parent();
    while let Some(dir) = cursor {
        match dir.file_name().and_then(|s| s.to_str()) {
            Some(name) if parse_ymd(name).is_some() => return Some(name.to_string()),
            _ => {}
        }
        cursor = dir.parent();
    }
    None
}

/// 提取 codex/zed 一行事件的日期，按多个候选位置回退。
pub(super) fn codex_like_event_date(v: &Value, payload: &Value) -> Option<String> {
    date_field(v.get("timestamp"))
        .or_else(|| date_field(payload.get("timestamp")))
        .or_else(|| date_field(payload.get("at")))
        .or_else(|| date_field(payload.get("started_at")))
        .or_else(|| date_field(payload.get("info").and_then(|info| info.get("at"))))
        .or_else(|| date_field(payload.get("info").and_then(|info| info.get("timestamp"))))
        .or_else(|| {
            date_field(
                payload
                    .get("info")
                    .and_then(|info| info.get("last_token_usage"))
                    .and_then(|last| last.get("at")),
            )
        })
        .or_else(|| {
            date_field(
                payload
                    .get("info")
                    .and_then(|info| info.get("last_token_usage"))
                    .and_then(|last| last.get("timestamp")),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::{SourceKind, consumed_jsonl_bytes};

    #[test]
    fn consumed_bytes_keeps_complete_tail_without_newline() {
        let bytes = br#"{"a":1}
{"b":2}"#;
        assert_eq!(consumed_jsonl_bytes(bytes), bytes.len() as u64);
    }

    #[test]
    fn consumed_bytes_stops_before_incomplete_tail() {
        let bytes = br#"{"a":1}
{"b":"#;
        assert_eq!(consumed_jsonl_bytes(bytes), 8);
    }

    #[test]
    fn append_scan_is_enabled_only_for_self_contained_jsonl_sources() {
        assert!(SourceKind::Claude.supports_append_scan());
        assert!(SourceKind::OmpSession.supports_append_scan());
        assert!(SourceKind::PiSession.supports_append_scan());
        assert!(!SourceKind::CodexLike("codex").supports_append_scan());
        assert!(!SourceKind::Copilot("copilot").supports_append_scan());
        assert!(!SourceKind::MimoSession.supports_append_scan());
    }
}
