//! ZCode CLI model-io rollout jsonl（~/.zcode/cli/rollout/）解析。
//!
//! 每行是一次完整的模型请求记录：
//! - `completedAt`：请求完成时间（ISO8601），作为日期与 timestamp 来源。
//! - `model.modelId` / `model.role`：模型名与角色（main / subagent）。
//! - usage 有两份口径，只取其一（见函数内注释）：首选
//!   `response.providerMetadata.<provider>.usage`（snake_case，单次请求增量）；
//!   回退 `response.usage`（camelCase，会话累计，需减去 cacheRead 还原增量）。
//! - `sessionId`：记录内会话 ID；缺失时回退文件名。
//! - session 粒度是文件级：主会话 `model-io-sess_<id>.jsonl`，subagent 会话
//!   `model-io-sess_subagent_agent_<id>.jsonl`。subagent 记录标记 `is_sidechain`。

use serde_json::Value;
use std::path::Path;

use super::{RawEntry, u64_field};
use crate::stats::date::{date_from_iso, timestamp_secs_from_iso};

const FILE_PREFIX: &str = "model-io-sess_";
const SUBAGENT_MARK: &str = "subagent_agent_";

pub(super) fn parse(content: &str, path: &Path) -> Vec<RawEntry> {
    let session_id = session_id_from_path(path);
    let sidechain_by_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|name| name.contains(SUBAGENT_MARK));

    let mut out = Vec::new();
    for line in content.lines() {
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };

        // usage 有两份口径，必须只取其一：
        // - `response.providerMetadata.anthropic.usage`（snake_case）：单次请求的原始
        //   增量，input_tokens 不含 cache_read。首选。
        // - `response.usage`（camelCase）：会话内累计值，inputTokens 包含
        //   cacheReadTokens 且随轮次累加。仅作回退，且需减去 cacheRead 还原增量。
        let provider_usage = v
            .get("response")
            .and_then(|r| r.get("providerMetadata"))
            .and_then(|pm| {
                if let Some(a) = pm.get("anthropic") {
                    return Some(a);
                }
                // 其他 provider（openai-compatible 等）也可能带原始 usage
                pm.as_object()?.values().find(|v| v.get("usage").is_some())
            })
            .and_then(|p| p.get("usage"));
        let usage = match provider_usage.filter(|u| !u.is_null()) {
            Some(u) => (
                u64_field(u, "input_tokens"),
                u64_field(u, "output_tokens"),
                u64_field(u, "cache_read_input_tokens"),
                u64_field(u, "cache_creation_input_tokens"),
            ),
            None => {
                let u = v
                    .get("response")
                    .and_then(|r| r.get("usage"))
                    .filter(|u| !u.is_null() && !u.as_object().is_some_and(|m| m.is_empty()));
                let Some(u) = u else { continue };
                let cache_read = u64_field(u, "cacheReadTokens");
                (
                    u64_field(u, "inputTokens").saturating_sub(cache_read),
                    u64_field(u, "outputTokens"),
                    cache_read,
                    u64_field(u, "cacheWriteTokens"),
                )
            }
        };
        let (input, output, cache_read, cache_write) = usage;
        if input == 0 && output == 0 && cache_read == 0 && cache_write == 0 {
            continue;
        }

        let Some(completed_at) = v
            .get("completedAt")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let date = date_from_iso(completed_at);
        if date.is_empty() {
            continue;
        }

        let model = v
            .get("model")
            .and_then(|m| m.get("modelId"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_lowercase)
            .unwrap_or_else(|| "unknown".to_string());
        let is_sidechain = sidechain_by_name
            || v.get("model")
                .and_then(|m| m.get("role"))
                .and_then(Value::as_str)
                .is_some_and(|role| role != "main");

        out.push(RawEntry {
            agent: "zcode".to_string(),
            model,
            date,
            input_tokens: input,
            output_tokens: output,
            cache_read_input_tokens: cache_read,
            cache_creation_input_tokens: cache_write,
            reasoning_output_tokens: 0,
            dedup_primary: None,
            dedup_secondary: None,
            is_sidechain,
            session_id: v
                .get("sessionId")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| session_id.clone()),
            message_id: None,
            timestamp_secs: timestamp_secs_from_iso(completed_at),
        });
    }
    out
}

fn session_id_from_path(path: &Path) -> Option<String> {
    let stem = path.file_stem().and_then(|s| s.to_str())?;
    let id = stem
        .strip_prefix(FILE_PREFIX)
        .map(|rest| {
            // `subagent_agent_<uuid>` → `<uuid>`，与主会话命名对齐
            rest.strip_prefix(SUBAGENT_MARK).unwrap_or(rest)
        })
        .unwrap_or(stem);
    Some(id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(content: &str, file: &str) -> Vec<RawEntry> {
        parse(content, Path::new("/tmp").join(file).as_path())
    }

    #[test]
    fn parses_main_session_provider_usage() {
        let content = concat!(
            r#"{"completedAt":"2026-09-02T09:28:37.376Z","sessionId":"sess_716a0e37","model":{"modelId":"GLM-5.3","role":"main"},"response":{"providerMetadata":{"anthropic":{"usage":{"input_tokens":36,"output_tokens":53,"cache_read_input_tokens":55872,"cache_creation_input_tokens":0}}}}}"#,
            "\n",
        );
        let r = p(content, "model-io-sess_716a0e37.jsonl");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].agent, "zcode");
        assert_eq!(r[0].model, "glm-5.3");
        assert_eq!(r[0].date, "2026-09-02");
        assert_eq!(r[0].input_tokens, 36);
        assert_eq!(r[0].output_tokens, 53);
        assert_eq!(r[0].cache_read_input_tokens, 55872);
        assert!(!r[0].is_sidechain);
        assert_eq!(r[0].session_id.as_deref(), Some("sess_716a0e37"));
        assert!(r[0].timestamp_secs.is_some());
    }

    #[test]
    fn marks_subagent_from_filename_and_normalizes_session() {
        let content = concat!(
            r#"{"completedAt":"2026-09-02T10:00:00Z","model":{"modelId":"glm-5.2","role":"main"},"response":{"providerMetadata":{"anthropic":{"usage":{"input_tokens":100,"output_tokens":10,"cache_read_input_tokens":0,"cache_creation_input_tokens":5}}}}}"#,
            "\n",
        );
        let r = p(content, "model-io-sess_subagent_agent_abc123.jsonl");
        assert_eq!(r.len(), 1);
        assert!(r[0].is_sidechain);
        // 记录内没有 sessionId，回退到文件名（去掉 subagent_agent_ 前缀）
        assert_eq!(r[0].session_id.as_deref(), Some("abc123"));
        assert_eq!(r[0].cache_creation_input_tokens, 5);
    }

    #[test]
    fn marks_subagent_from_role_field() {
        let content = concat!(
            r#"{"completedAt":"2026-09-02T10:00:00Z","model":{"modelId":"glm-5.2","role":"subagent"},"response":{"providerMetadata":{"anthropic":{"usage":{"input_tokens":1,"output_tokens":1}}}}}"#,
            "\n",
        );
        let r = p(content, "model-io-sess_716a0e37.jsonl");
        assert_eq!(r.len(), 1);
        assert!(r[0].is_sidechain);
    }

    #[test]
    fn falls_back_to_cumulative_camel_usage_and_subtracts_cache_read() {
        // camelCase usage 是会话累计：inputTokens 包含 cacheReadTokens，需做差还原增量
        let content = concat!(
            r#"{"completedAt":"2026-09-02T10:00:00Z","model":{"modelId":"glm-5.2"},"response":{"usage":{"inputTokens":56170,"outputTokens":474,"cacheReadTokens":56064,"cacheWriteTokens":0}}}"#,
            "\n",
        );
        let r = p(content, "model-io-sess_x.jsonl");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].input_tokens, 106);
        assert_eq!(r[0].output_tokens, 474);
        assert_eq!(r[0].cache_read_input_tokens, 56064);
    }

    #[test]
    fn skips_lines_without_usage_or_timestamp() {
        let content = concat!(
            r#"{"completedAt":"2026-09-02T10:00:00Z","model":{"modelId":"glm-5.2"},"response":{"usage":{}}}"#,
            "\n",
            r#"{"completedAt":"2026-09-02T10:00:00Z","model":{"modelId":"glm-5.2"}}"#,
            "\n",
            r#"{"response":{"providerMetadata":{"anthropic":{"usage":{"input_tokens":1,"output_tokens":1}}}}}"#,
            "\n",
            r#"{"completedAt":"2026-09-02T10:00:00Z","response":{"providerMetadata":{"anthropic":{"usage":{"input_tokens":0,"output_tokens":0}}}}}"#,
            "\n",
            "not json\n",
        );
        let r = p(content, "model-io-sess_x.jsonl");
        assert!(r.is_empty());
    }

    #[test]
    fn unknown_model_falls_back() {
        let content = concat!(
            r#"{"completedAt":"2026-09-02T10:00:00Z","response":{"providerMetadata":{"anthropic":{"usage":{"input_tokens":1,"output_tokens":1}}}}}"#,
            "\n",
        );
        let r = p(content, "model-io-sess_x.jsonl");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].model, "unknown");
    }
}
