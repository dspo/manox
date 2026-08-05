//! Codex / Zed agent 共用的 jsonl 格式解析。
//!
//! 参考 ccusage `adapter/codex/parser.rs`，关键点：
//! - 事件 `{type:"event_msg", payload:{type:"token_count", info:{total_token_usage, last_token_usage}}}`。
//! - **优先 `last_token_usage`**；若该字段缺失，用 `total_token_usage - prev_total` 兜底。
//! - `cached_input_tokens` 钳制为 `min(input_tokens)`，避免历史版本中 cached > input 的脏数据。
//! - `reasoning_output_tokens` 已包含在 `output_tokens` 内，仅作展示，不参与汇总。
//! - 早期 codex 没有 `turn_context.model`，回退为 `gpt-5`（标记 fallback）。
//! - 同时支持 ccusage 里的 headless/exec JSONL：usage 可在 root/data/result/response，
//!   token 字段支持 input_tokens/prompt_tokens/input、output_tokens/completion_tokens/output。
//!
//! ## session_id 的来源
//!
//! 跨文件去重需要稳定的 session_id：同一逻辑 session 在 resume 后会写到新 rollout 文件，
//! 文件名 UUID 不同。所以 session_id 必须从 jsonl 第一条 `session_meta.payload.id` 取
//! （codex CLI / Zed 都在 session_meta 里写了 UUID）。文件名 stem 仅作兜底。

use serde_json::Value;
use std::path::Path;

use super::{RawEntry, codex_like_event_date, u64_field};
use crate::stats::date::date_from_iso;
use crate::stats::format;

const FALLBACK_MODEL: &str = "gpt-5";

pub(super) fn parse(
    content: &str,
    agent: &str,
    fallback_date: Option<&str>,
    path: &Path,
) -> Vec<RawEntry> {
    let mut session_id: Option<String> = None;
    let mut current_model: Option<String> = None;
    let mut current_date: Option<String> = None;
    let mut prev_total: Option<TotalTokens> = None;
    let mut out: Vec<RawEntry> = Vec::new();

    for line in content.lines() {
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let typ = v.get("type").and_then(Value::as_str).unwrap_or("");
        let Some(payload) = v.get("payload") else {
            if let Some(entry) = parse_headless(
                &v,
                agent,
                fallback_date,
                path,
                &mut session_id,
                &mut current_model,
            ) {
                out.push(entry);
            }
            continue;
        };

        let event_date = codex_like_event_date(&v, payload);
        if let Some(date) = &event_date {
            current_date = Some(date.clone());
        }

        fn parse_headless(
            v: &Value,
            agent: &str,
            fallback_date: Option<&str>,
            path: &Path,
            session_id: &mut Option<String>,
            current_model: &mut Option<String>,
        ) -> Option<RawEntry> {
            let usage_value = usage_container(v)?;
            let usage = read_usage_aliases(usage_value);
            if usage.input_tokens == 0
                && usage.cached_input_tokens == 0
                && usage.cache_creation_input_tokens == 0
                && usage.output_tokens == 0
                && usage.reasoning_output_tokens == 0
            {
                return None;
            }

            if let Some(model) = model_from_value(v) {
                *current_model = Some(model);
            }
            let model = current_model
                .clone()
                .unwrap_or_else(|| FALLBACK_MODEL.to_string());
            let timestamp = timestamp_from_value(v).unwrap_or_default();
            let date = if timestamp.is_empty() {
                fallback_date.map(str::to_string)
            } else {
                let date = date_from_iso(&timestamp);
                (!date.is_empty()).then_some(date)
            }?;
            if session_id.is_none() {
                *session_id = Some(derive_session_id_from_path(path));
            }
            let sid = session_id
                .clone()
                .unwrap_or_else(|| derive_session_id_from_path(path));
            let dedup_timestamp = if timestamp.is_empty() {
                date.clone()
            } else {
                timestamp
            };
            let cached = usage.cached_input_tokens.min(usage.input_tokens);

            Some(RawEntry {
                agent: agent.to_string(),
                model,
                date,
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                cache_read_input_tokens: cached,
                cache_creation_input_tokens: usage.cache_creation_input_tokens,
                reasoning_output_tokens: usage.reasoning_output_tokens,
                dedup_primary: Some(format!("{sid}|{dedup_timestamp}")),
                dedup_secondary: Some(format!(
                    "{}/{}/{}/{}/{}",
                    usage.input_tokens,
                    cached,
                    usage.cache_creation_input_tokens,
                    usage.output_tokens,
                    usage.reasoning_output_tokens,
                )),
                is_sidechain: false,
                session_id: Some(sid.clone()),
                message_id: None,
                timestamp_secs: None,
            })
        }

        match typ {
            "session_meta" => {
                if session_id.is_none() {
                    session_id = payload
                        .get("id")
                        .and_then(Value::as_str)
                        .or_else(|| payload.get("session_id").and_then(Value::as_str))
                        .map(str::to_string);
                }
                if let Some(m) = payload.get("model").and_then(Value::as_str) {
                    current_model = Some(m.to_string());
                }
                continue;
            }
            "turn_context" => {
                if let Some(m) = payload.get("model").and_then(Value::as_str) {
                    current_model = Some(m.to_string());
                }
                continue;
            }
            "event_msg" => {}
            _ => continue,
        }

        if payload.get("type").and_then(Value::as_str) != Some("token_count") {
            continue;
        }
        let info = match payload.get("info") {
            Some(i) if !i.is_null() => i,
            _ => continue,
        };

        let total = info
            .get("total_token_usage")
            .filter(|v| !v.is_null())
            .map(read_total);
        let last = info.get("last_token_usage").filter(|v| !v.is_null());

        // last 优先；缺失则 total - prev_total。参见 ccusage parser.rs:138-148
        let usage = if let Some(last) = last {
            Usage {
                input_tokens: u64_field(last, "input_tokens"),
                cached_input_tokens: u64_field(last, "cached_input_tokens"),
                cache_creation_input_tokens: u64_field(last, "cache_creation_input_tokens"),
                output_tokens: u64_field(last, "output_tokens"),
                reasoning_output_tokens: u64_field(last, "reasoning_output_tokens"),
            }
        } else {
            let Some(total) = total else { continue };
            let prev = prev_total.unwrap_or_default();
            Usage {
                input_tokens: total.input.saturating_sub(prev.input),
                cached_input_tokens: total.cached.saturating_sub(prev.cached),
                cache_creation_input_tokens: total.cache_create.saturating_sub(prev.cache_create),
                output_tokens: total.output.saturating_sub(prev.output),
                reasoning_output_tokens: total.reasoning.saturating_sub(prev.reasoning),
            }
        };

        if let Some(total) = total {
            prev_total = Some(total);
        }

        // cached 钳制（ccusage parser.rs:182）
        let cached = usage.cached_input_tokens.min(usage.input_tokens);

        if usage.input_tokens == 0
            && cached == 0
            && usage.cache_creation_input_tokens == 0
            && usage.output_tokens == 0
        {
            continue;
        }

        let date = event_date
            .or_else(|| current_date.clone())
            .or_else(|| fallback_date.map(str::to_string))
            .unwrap_or_default();
        if date.is_empty() {
            continue;
        }
        let model = current_model
            .clone()
            .unwrap_or_else(|| FALLBACK_MODEL.to_string());

        // 跨文件去重 key：同一 session_id（来自 session_meta.payload.id）下，
        // 同 timestamp + 同一组 token 数的事件视为重复，参见 ccusage codex_event_key 元组。
        let timestamp = v
            .get("timestamp")
            .and_then(Value::as_str)
            .or_else(|| payload.get("at").and_then(Value::as_str))
            .or_else(|| payload.get("timestamp").and_then(Value::as_str))
            .unwrap_or("")
            .to_string();
        let sid = session_id
            .clone()
            .unwrap_or_else(|| derive_session_id_from_path(path));
        let dedup_primary = Some(format!("{sid}|{timestamp}"));
        let dedup_secondary = Some(format!(
            "{}/{}/{}/{}/{}",
            usage.input_tokens,
            cached,
            usage.cache_creation_input_tokens,
            usage.output_tokens,
            usage.reasoning_output_tokens,
        ));

        out.push(RawEntry {
            agent: agent.to_string(),
            model,
            date,
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_read_input_tokens: cached,
            cache_creation_input_tokens: usage.cache_creation_input_tokens,
            reasoning_output_tokens: usage.reasoning_output_tokens,
            dedup_primary,
            dedup_secondary,
            is_sidechain: false,
            session_id: Some(sid.clone()),
            message_id: None,
            timestamp_secs: None,
        });
    }

    out
}

#[derive(Default, Clone, Copy)]
struct TotalTokens {
    input: u64,
    cached: u64,
    cache_create: u64,
    output: u64,
    reasoning: u64,
}

fn read_total(v: &Value) -> TotalTokens {
    TotalTokens {
        input: u64_field(v, "input_tokens"),
        cached: u64_field(v, "cached_input_tokens"),
        cache_create: u64_field(v, "cache_creation_input_tokens"),
        output: u64_field(v, "output_tokens"),
        reasoning: u64_field(v, "reasoning_output_tokens"),
    }
}

struct Usage {
    input_tokens: u64,
    cached_input_tokens: u64,
    cache_creation_input_tokens: u64,
    output_tokens: u64,
    reasoning_output_tokens: u64,
}

fn usage_container(v: &Value) -> Option<&Value> {
    v.get("usage")
        .or_else(|| v.get("data").and_then(|x| x.get("usage")))
        .or_else(|| v.get("result").and_then(|x| x.get("usage")))
        .or_else(|| v.get("response").and_then(|x| x.get("usage")))
}

fn read_usage_aliases(v: &Value) -> Usage {
    Usage {
        input_tokens: u64_first(v, &["input_tokens", "prompt_tokens", "input"]),
        cached_input_tokens: u64_first(
            v,
            &[
                "cached_input_tokens",
                "cache_read_input_tokens",
                "cached_tokens",
            ],
        ),
        cache_creation_input_tokens: u64_field(v, "cache_creation_input_tokens"),
        output_tokens: u64_first(v, &["output_tokens", "completion_tokens", "output"]),
        reasoning_output_tokens: u64_first(v, &["reasoning_output_tokens", "reasoning_tokens"]),
    }
}

fn u64_first(v: &Value, keys: &[&str]) -> u64 {
    keys.iter()
        .filter_map(|key| number_value(v.get(*key)))
        .find(|n| *n > 0)
        .unwrap_or_default()
}

fn number_value(value: Option<&Value>) -> Option<u64> {
    match value? {
        Value::Number(n) => n
            .as_u64()
            .or_else(|| n.as_i64().and_then(|i| (i >= 0).then_some(i as u64))),
        Value::String(s) => s.trim().parse::<u64>().ok(),
        _ => None,
    }
}

fn model_from_value(v: &Value) -> Option<String> {
    model_from_container(v)
        .or_else(|| v.get("data").and_then(model_from_container))
        .or_else(|| v.get("result").and_then(model_from_container))
        .or_else(|| v.get("response").and_then(model_from_container))
}

fn model_from_container(v: &Value) -> Option<String> {
    string_field(v, "model")
        .or_else(|| string_field(v, "model_name"))
        .or_else(|| v.get("metadata").and_then(|m| string_field(m, "model")))
}

fn string_field(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).and_then(|s| {
        let s = s.trim();
        (!s.is_empty()).then(|| s.to_string())
    })
}

fn timestamp_from_value(v: &Value) -> Option<String> {
    timestamp_from_container(v)
        .or_else(|| v.get("data").and_then(timestamp_from_container))
        .or_else(|| v.get("result").and_then(timestamp_from_container))
        .or_else(|| v.get("response").and_then(timestamp_from_container))
}

fn timestamp_from_container(v: &Value) -> Option<String> {
    timestamp_field(v.get("timestamp"))
        .or_else(|| timestamp_field(v.get("created_at")))
        .or_else(|| timestamp_field(v.get("createdAt")))
}

fn timestamp_field(value: Option<&Value>) -> Option<String> {
    let value = value?;
    if let Some(text) = value.as_str() {
        let text = text.trim();
        return (!text.is_empty()).then(|| text.to_string());
    }
    let raw = number_value(Some(value))?;
    let ms = if raw > 10_000_000_000 {
        raw
    } else {
        raw.checked_mul(1_000)?
    };
    Some(format::iso_from_unix_ms(ms.min(i64::MAX as u64) as i64))
}

fn derive_session_id_from_path(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| path.to_string_lossy().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(content: &str, agent: &str) -> Vec<RawEntry> {
        parse(content, agent, None, Path::new("/tmp/session-x.jsonl"))
    }

    #[test]
    fn prefers_last_over_total_when_both_present() {
        let content = concat!(
            r#"{"type":"turn_context","payload":{"model":"qwen3.6-plus"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"token_count","at":"2026-05-27T12:34:56Z","info":{"total_token_usage":{"input_tokens":500,"cached_input_tokens":100,"output_tokens":50},"last_token_usage":{"input_tokens":100,"cached_input_tokens":20,"cache_creation_input_tokens":5,"output_tokens":7}}}}"#,
            "\n",
        );
        let r = p(content, "codex");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].input_tokens, 100);
        assert_eq!(r[0].output_tokens, 7);
        assert_eq!(r[0].cache_read_input_tokens, 20);
        assert_eq!(r[0].cache_creation_input_tokens, 5);
    }

    #[test]
    fn falls_back_to_total_diff_when_last_missing() {
        let content = concat!(
            r#"{"type":"turn_context","payload":{"model":"gpt-5.4"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"token_count","at":"2026-05-27T12:34:56Z","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":20,"output_tokens":7}}}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"token_count","at":"2026-05-27T12:35:56Z","info":{"total_token_usage":{"input_tokens":150,"cached_input_tokens":30,"output_tokens":17}}}}"#,
            "\n",
        );
        let r = p(content, "codex");
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].input_tokens, 100);
        assert_eq!(r[1].input_tokens, 50);
        assert_eq!(r[1].output_tokens, 10);
    }

    #[test]
    fn clamps_cached_to_input() {
        // 历史版本中 cached > input 的脏样本
        let content = concat!(
            r#"{"type":"turn_context","payload":{"model":"gpt-5"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"token_count","at":"2026-05-27T12:34:56Z","info":{"last_token_usage":{"input_tokens":50,"cached_input_tokens":80,"output_tokens":10}}}}"#,
            "\n",
        );
        let r = p(content, "codex");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].input_tokens, 50);
        assert_eq!(r[0].cache_read_input_tokens, 50);
    }

    #[test]
    fn falls_back_to_path_date_when_timestamps_missing() {
        let content = concat!(
            r#"{"type":"session_meta","payload":{"session_id":"zed-abc","agent":"zed","model":"qwen3.6-plus","started_at":"2026-05-28T09:00:00Z"}}"#,
            "\n",
            r#"{"type":"turn_context","payload":{"model":"qwen3.6-plus"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":11,"output_tokens":13}}}}"#,
            "\n",
        );
        let r = parse(
            content,
            "zed",
            Some("2026-05-27"),
            Path::new("/tmp/zed-abc.jsonl"),
        );
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].date, "2026-05-28");
        assert_eq!(r[0].input_tokens, 11);
        assert_eq!(r[0].output_tokens, 13);
    }

    #[test]
    fn falls_back_to_gpt5_when_model_unknown() {
        let content = concat!(
            r#"{"type":"event_msg","payload":{"type":"token_count","at":"2026-05-27T12:34:56Z","info":{"last_token_usage":{"input_tokens":1,"output_tokens":1}}}}"#,
            "\n",
        );
        let r = p(content, "codex");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].model, FALLBACK_MODEL);
    }

    #[test]
    fn dedup_primary_uses_session_meta_id_not_file_stem() {
        // 同一逻辑 session resume 到新 rollout 文件：session_meta.payload.id 不变，
        // 文件名 stem 变。dedup_primary 必须用 session_meta.id 才能跨文件合并重复事件。
        let content = concat!(
            r#"{"type":"session_meta","payload":{"id":"sess-A","model":"gpt-5"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"token_count","at":"2026-05-27T12:34:56Z","info":{"last_token_usage":{"input_tokens":1,"output_tokens":1}}}}"#,
            "\n",
        );
        let r1 = parse(content, "codex", None, Path::new("/tmp/file-1.jsonl"));
        let r2 = parse(content, "codex", None, Path::new("/tmp/file-2.jsonl"));
        assert_eq!(r1.len(), 1);
        assert_eq!(r2.len(), 1);
        assert!(
            r1[0]
                .dedup_primary
                .as_deref()
                .unwrap()
                .starts_with("sess-A|")
        );
        assert_eq!(r1[0].dedup_primary, r2[0].dedup_primary);
        assert_eq!(r1[0].dedup_secondary, r2[0].dedup_secondary);
    }

    #[test]
    fn parses_headless_usage_at_root_with_aliases() {
        let content = concat!(
            r#"{"timestamp":"2026-05-27T12:34:56Z","model_name":"gpt-5.4","usage":{"prompt_tokens":"120","cached_tokens":30,"completion_tokens":9,"reasoning_tokens":2}}"#,
            "\n",
        );
        let r = p(content, "codex");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].date, "2026-05-27");
        assert_eq!(r[0].model, "gpt-5.4");
        assert_eq!(r[0].input_tokens, 120);
        assert_eq!(r[0].cache_read_input_tokens, 30);
        assert_eq!(r[0].output_tokens, 9);
        assert_eq!(r[0].reasoning_output_tokens, 2);
    }

    #[test]
    fn parses_headless_usage_from_nested_result() {
        let content = concat!(
            r#"{"result":{"createdAt":"2026-05-27T12:34:56Z","metadata":{"model":"gpt-5.5"},"usage":{"input":10,"output":3}}}"#,
            "\n",
        );
        let r = p(content, "codex");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].model, "gpt-5.5");
        assert_eq!(r[0].input_tokens, 10);
        assert_eq!(r[0].output_tokens, 3);
    }
}
