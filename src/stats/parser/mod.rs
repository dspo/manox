//! 各 agent 的日志解析。
//!
//! 解析阶段产出"未去重"的 [`RawEntry`]，由上层 `mod.rs` 在所有文件解析完之后做
//! 跨文件去重再聚合到 [`UsageRecord`]。
//!
//! 设计参考 ccusage `rust/crates/ccusage/src/adapter/{claude,codex,copilot}/`。

pub(super) mod claude;
pub(super) mod codex;
pub(super) mod omp;
pub(super) mod omp_session;
pub(super) mod copilot;

use serde::{Deserialize, Serialize};
use serde_json::Value;
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
    /// - claude: None（Claude Code Stats raw-sum，不按 message id 去重）
    /// - codex/zed: `session_id|timestamp_iso`
    /// - copilot: 见 copilot 内部说明
    pub(super) dedup_primary: Option<String>,
    /// 次级主键（与 primary 联合一起去重）。
    /// - claude: None
    /// - codex 系列: 一组 token 数，让重复 snapshot 落到同一 key
    pub(super) dedup_secondary: Option<String>,
    /// 是否为 sidechain 记录（仅 claude 使用）。
    #[serde(default)]
    pub(super) is_sidechain: bool,
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
}

pub(super) fn parse_file(path: &Path, kind: SourceKind) -> Vec<RawEntry> {
    let content = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    match kind {
        SourceKind::Claude => claude::parse(&content),
        SourceKind::CodexLike(agent) => {
            let fallback_date = fallback_date_from_path(path);
            codex::parse(&content, agent, fallback_date.as_deref(), path)
        }
        SourceKind::Copilot(agent) => copilot::parse(&content, agent, path),
        SourceKind::OmpSession => omp_session::parse(&content),
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
