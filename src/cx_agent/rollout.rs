//! Jsonl rollout 持久化。
//!
//! 写入 `~/.local/share/cx/cx-agent-sessions/<YYYY-MM-DD>/<session-id>.jsonl`，
//! 结构对齐 codex（`{type:"event_msg", payload:{type:"token_count", info:{last_token_usage:{...}}}}`
//! + `{type:"turn_context", payload:{model:"..."}}`），并补齐顶层/载荷时间戳，
//!   便于复用 `cx stats` 解析。

use std::fs::{File, OpenOptions, create_dir_all};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::cx_agent::config::ProviderAdapterConfig;

const SESSIONS_DIR: &str = "cx-agent-sessions";

pub struct Rollout {
    writer: BufWriter<File>,
    pub session_id: String,
    pub path: PathBuf,
}

impl Rollout {
    pub fn open(adapter: &ProviderAdapterConfig) -> Result<Self> {
        let dir = base_dir()?;
        let day = format_today();
        let day_dir = dir.join(&day);
        create_dir_all(&day_dir)
            .with_context(|| format!("创建 rollout 目录失败: {}", day_dir.display()))?;

        let session_id = format!("cx-agent-{}", short_id(8));
        let path = day_dir.join(format!("{}.jsonl", session_id));
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("打开 rollout 文件失败: {}", path.display()))?;
        let writer = BufWriter::new(file);

        let mut rollout = Self {
            writer,
            session_id,
            path,
        };
        rollout.write_session_header(adapter)?;
        rollout.write_turn_context(&adapter.model_id, &adapter.provider_name)?;
        Ok(rollout)
    }

    fn write_session_header(&mut self, adapter: &ProviderAdapterConfig) -> Result<()> {
        let timestamp = iso_now();
        self.write_record(&serde_json::json!({
            "type": "session_meta",
            "timestamp": timestamp,
            "payload": {
                "session_id": self.session_id,
                "agent": "cx-agent",
                "provider": adapter.provider_name,
                "model": adapter.model_id,
                "wire_api": adapter.wire_api.display(),
                "started_at": timestamp,
            }
        }))
    }

    pub fn write_turn_context(&mut self, model_id: &str, provider: &str) -> Result<()> {
        let timestamp = iso_now();
        self.write_record(&serde_json::json!({
            "type": "turn_context",
            "timestamp": timestamp,
            "payload": {
                "model": model_id,
                "provider": provider,
                "agent": "cx-agent",
                "at": timestamp,
            }
        }))
    }

    pub fn write_user_message(&mut self, text: &str) -> Result<()> {
        let timestamp = iso_now();
        self.write_record(&serde_json::json!({
            "type": "event_msg",
            "timestamp": timestamp,
            "payload": {
                "type": "user_message",
                "text": text,
                "at": timestamp,
            }
        }))
    }

    pub fn write_assistant_message(&mut self, text: &str) -> Result<()> {
        let timestamp = iso_now();
        self.write_record(&serde_json::json!({
            "type": "event_msg",
            "timestamp": timestamp,
            "payload": {
                "type": "assistant_message",
                "text": text,
                "at": timestamp,
            }
        }))
    }

    pub fn write_token_usage(&mut self, usage: TokenUsageRecord) -> Result<()> {
        let timestamp = iso_now();
        self.write_record(&token_usage_record(usage, &timestamp))
    }

    pub fn write_error(&mut self, msg: &str) -> Result<()> {
        let timestamp = iso_now();
        self.write_record(&serde_json::json!({
            "type": "event_msg",
            "timestamp": timestamp,
            "payload": {
                "type": "error",
                "message": msg,
                "at": timestamp,
            }
        }))
    }

    fn write_record<T: Serialize>(&mut self, value: &T) -> Result<()> {
        let line = serde_json::to_string(value).context("序列化 rollout 行失败")?;
        self.writer.write_all(line.as_bytes())?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TokenUsageRecord {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub reasoning: u64,
}

fn token_usage_record(usage: TokenUsageRecord, timestamp: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "event_msg",
        "timestamp": timestamp,
        "payload": {
            "type": "token_count",
            "at": timestamp,
            "info": {
                "at": timestamp,
                "last_token_usage": {
                    "input_tokens": usage.input,
                    "output_tokens": usage.output,
                    "cached_input_tokens": usage.cache_read,
                    "cache_creation_input_tokens": usage.cache_write,
                    "reasoning_tokens": usage.reasoning,
                }
            }
        }
    })
}

pub fn base_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("无法解析用户主目录")?;
    Ok(home.join(".local/share/cx").join(SESSIONS_DIR))
}

fn iso_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{}Z", epoch_to_iso(secs))
}

fn format_today() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, mo, d) = epoch_to_ymd(secs);
    format!("{y:04}-{mo:02}-{d:02}")
}

fn short_id(n: usize) -> String {
    let mut buf = vec![0u8; n];
    rand::Rng::fill(&mut rand::thread_rng(), &mut buf[..]);
    base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, buf)
}

// ── 简易日期换算（避免引入 chrono；UTC、忽略闰秒；适合日志命名）─────────

fn epoch_to_ymd(secs: u64) -> (u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let (y, mo, d) = days_to_ymd(days);
    (y as u32, mo as u32, d as u32)
}

fn epoch_to_iso(secs: u64) -> String {
    let (y, mo, d) = epoch_to_ymd(secs);
    let h = (secs / 3_600 % 24) as u32;
    let m = (secs / 60 % 60) as u32;
    let s = (secs % 60) as u32;
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}")
}

fn days_to_ymd(mut days: i64) -> (i64, i64, i64) {
    // 基于 1970-01-01；正向爬。仅用于日志路径，1900~2400 内可信。
    let mut year = 1970i64;
    loop {
        let dy = if is_leap(year) { 366 } else { 365 };
        if days < dy {
            break;
        }
        days -= dy;
        year += 1;
    }
    let months = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut mo = 1i64;
    for (i, &dm) in months.iter().enumerate() {
        let dm = if i == 1 && is_leap(year) { 29 } else { dm };
        if days < dm as i64 {
            break;
        }
        days -= dm as i64;
        mo += 1;
    }
    (year, mo, days + 1)
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_usage_record_includes_top_level_and_payload_timestamps() {
        let usage = TokenUsageRecord {
            input: 10,
            output: 20,
            cache_read: 3,
            cache_write: 4,
            reasoning: 5,
        };

        let value = token_usage_record(usage, "2026-05-27T10:11:12Z");

        assert_eq!(
            value.get("type").and_then(|v| v.as_str()),
            Some("event_msg")
        );
        assert_eq!(
            value.get("timestamp").and_then(|v| v.as_str()),
            Some("2026-05-27T10:11:12Z")
        );
        assert_eq!(
            value
                .get("payload")
                .and_then(|v| v.get("at"))
                .and_then(|v| v.as_str()),
            Some("2026-05-27T10:11:12Z")
        );
        assert_eq!(
            value
                .get("payload")
                .and_then(|v| v.get("info"))
                .and_then(|v| v.get("at"))
                .and_then(|v| v.as_str()),
            Some("2026-05-27T10:11:12Z")
        );
        assert_eq!(
            value
                .get("payload")
                .and_then(|v| v.get("info"))
                .and_then(|v| v.get("last_token_usage"))
                .and_then(|v| v.get("cached_input_tokens"))
                .and_then(|v| v.as_u64()),
            Some(3)
        );
    }
}
