//! deepseek-harness (`dsh`) session transcript parsing.
//!
//! dsh persists each session as one append-only logical JSONL log under
//! `$DSH_HOME/sessions/<project>/<session-id>/`: `session.jsonl.zstd` by
//! default (a concatenation of independent zstd frames — a checksummed
//! header frame followed by one frame per appended batch) or plain
//! `session.jsonl` with `compression: 'none'`. Logical lines:
//! - `type: "session"` header line providing the session id
//! - `type: "assistant/message"` events carrying the step's `usage`
//! - everything else (turn/step boundaries, tool pairs, request snapshots,
//!   packed-chunk rows like `text-chunks`) carries no per-message usage
//!
//! Accounting follows dsh's disjoint `TokenUsage` buckets: `inputTokens`
//! excludes cache hits (billed input = input + cacheRead + cacheWrite), and
//! `reasoningTokens` are already folded into `outputTokens`.
//!
//! Caliber: raw-sum (no message-id dedup), consistent with pi/omp/claude.

use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;

use super::RawEntry;
use crate::stats::AGENT_DSH;
use crate::stats::date::date_from_unix_secs;

/// Parse one dsh session transcript. `.zstd` artifacts decode as a
/// concatenation of independent zstd frames; plain artifacts read verbatim.
/// Both share the same logical lines.
pub(super) fn parse(path: &Path) -> Result<Vec<RawEntry>> {
    let bytes =
        std::fs::read(path).with_context(|| format!("读取日志失败 ({})", path.display()))?;
    let content = if path.extension().and_then(|s| s.to_str()) == Some("zstd") {
        let mut decoder = zstd::stream::read::Decoder::new(&bytes[..])
            .with_context(|| format!("zstd 解码失败 ({})", path.display()))?;
        let mut text = String::new();
        decoder
            .read_to_string(&mut text)
            .with_context(|| format!("zstd 解码失败 ({})", path.display()))?;
        text
    } else {
        String::from_utf8_lossy(&bytes).into_owned()
    };
    Ok(parse_content(&content))
}

fn parse_content(content: &str) -> Vec<RawEntry> {
    let mut out: Vec<RawEntry> = Vec::new();
    let mut session_id: Option<String> = None;

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match v.get("type").and_then(Value::as_str) {
            Some("session") => {
                if let Some(id) = v.get("id").and_then(Value::as_str) {
                    session_id = Some(id.to_string());
                }
            }
            Some("assistant/message") => {
                if let Some(entry) = parse_usage_event(&v, session_id.as_deref()) {
                    out.push(entry);
                }
            }
            // Turn/step boundaries, tool pairs, request snapshots, and
            // packed-chunk rows (`text-chunks` / `reasoning-chunks` /
            // `tool-call-chunks`) carry no per-message usage.
            _ => {}
        }
    }
    out
}

fn parse_usage_event(v: &Value, session_id: Option<&str>) -> Option<RawEntry> {
    let data = v.get("data")?;
    let usage = data.get("usage")?;
    if usage.is_null() {
        return None;
    }

    let input_tokens = usage
        .get("inputTokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output_tokens = usage
        .get("outputTokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cache_read = usage
        .get("cacheReadTokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cache_write = usage
        .get("cacheWriteTokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let reasoning = usage
        .get("reasoningTokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);

    if input_tokens == 0 && output_tokens == 0 && cache_read == 0 && cache_write == 0 {
        return None;
    }

    let model = data
        .pointer("/message/source/model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|m| !m.is_empty())?;

    // dsh event timestamps are Unix epoch milliseconds; the date bucket uses
    // the local timezone like every other source.
    let time_ms = v.get("time").and_then(Value::as_i64)?;
    let date = date_from_unix_secs(time_ms / 1000);
    if date.is_empty() {
        return None;
    }

    Some(RawEntry {
        agent: AGENT_DSH.to_string(),
        model: model.to_string(),
        date,
        input_tokens,
        output_tokens,
        cache_read_input_tokens: cache_read,
        cache_creation_input_tokens: cache_write,
        reasoning_output_tokens: reasoning,
        dedup_primary: None,
        dedup_secondary: None,
        is_sidechain: false,
        session_id: session_id.map(str::to_string),
        message_id: data
            .pointer("/message/id")
            .and_then(Value::as_str)
            .map(str::to_string),
        timestamp_secs: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const HEADER: &str = r#"{"type":"session","version":0,"id":"session-abc","cwd":"/p","createdAt":1755000000000,"delegationDepth":0}"#;

    /// The four disjoint dsh usage buckets plus the reasoning mirror column.
    struct Buckets {
        input: u64,
        output: u64,
        cache_read: u64,
        cache_write: u64,
        reasoning: u64,
    }

    /// One `assistant/message` event line in the on-disk shape.
    fn usage_line(seq: u64, time_ms: u64, model: &str, b: Buckets) -> String {
        let Buckets {
            input,
            output,
            cache_read,
            cache_write,
            reasoning,
        } = b;
        format!(
            r#"{{"type":"assistant/message","seq":{seq},"time":{time_ms},"data":{{"turn":1,"step":1,"message":{{"id":"msg-{seq}","role":"assistant","content":[],"source":{{"kind":"model","provider":"dashscope-completions","model":"{model}"}}}},"usage":{{"inputTokens":{input},"outputTokens":{output},"cacheReadTokens":{cache_read},"cacheWriteTokens":{cache_write},"reasoningTokens":{reasoning}}}}}}}"#
        )
    }

    #[test]
    fn parses_usage_and_skips_non_usage_lines() {
        let content = format!(
            "{HEADER}\n{}\n{}\n{}\n",
            usage_line(
                7,
                1_755_000_000_000,
                "qwen3.8-max",
                Buckets {
                    input: 832,
                    output: 109,
                    cache_read: 2048,
                    cache_write: 500,
                    reasoning: 20,
                },
            ),
            r#"{"type":"text-chunks","seq0":1,"time0":1755000000000,"members":[]}"#,
            r#"{"type":"tool/call","seq":9,"time":1755000000100,"data":{"turn":1,"step":1,"callId":"c1","name":"fs-read","arguments":"{}"}}"#,
        );
        let entries = parse_content(&content);
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.agent, "dsh");
        assert_eq!(e.model, "qwen3.8-max");
        assert_eq!(e.input_tokens, 832);
        assert_eq!(e.output_tokens, 109);
        assert_eq!(e.cache_read_input_tokens, 2048);
        assert_eq!(e.cache_creation_input_tokens, 500);
        assert_eq!(e.reasoning_output_tokens, 20);
        assert_eq!(e.session_id.as_deref(), Some("session-abc"));
        assert_eq!(e.message_id.as_deref(), Some("msg-7"));
        assert_eq!(
            e.date,
            date_from_unix_secs(1_755_000_000),
            "date buckets by local timezone"
        );
        assert!(e.dedup_primary.is_none(), "raw-sum caliber");
    }

    /// The on-disk artifact is a concatenation of independent zstd frames
    /// (header frame + one frame per append batch); decoding must span all
    /// of them and the header's session id must reach later frames' entries.
    #[test]
    fn parses_multi_frame_zstd_artifact() {
        let frame1 = format!(
            "{HEADER}\n{}\n",
            usage_line(
                1,
                1_755_000_000_000,
                "m1",
                Buckets {
                    input: 100,
                    output: 50,
                    cache_read: 0,
                    cache_write: 0,
                    reasoning: 0,
                },
            )
        );
        let frame2 = format!(
            "{}\n",
            usage_line(
                5,
                1_755_003_600_000,
                "m1",
                Buckets {
                    input: 200,
                    output: 60,
                    cache_read: 10,
                    cache_write: 5,
                    reasoning: 0,
                },
            )
        );
        let mut bytes = Vec::new();
        for frame in [&frame1, &frame2] {
            let mut enc = zstd::stream::write::Encoder::new(Vec::new(), 0).unwrap();
            enc.write_all(frame.as_bytes()).unwrap();
            bytes.extend_from_slice(&enc.finish().unwrap());
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl.zstd");
        std::fs::write(&path, &bytes).unwrap();

        let entries = parse(&path).unwrap();
        assert_eq!(entries.len(), 2, "both frames decode");
        assert_eq!(entries[0].input_tokens, 100);
        assert_eq!(entries[1].input_tokens, 200);
        assert!(
            entries
                .iter()
                .all(|e| e.session_id.as_deref() == Some("session-abc")),
            "header session id spans frames"
        );
    }

    #[test]
    fn parses_plain_jsonl_artifact() {
        let content = format!(
            "{HEADER}\n{}\n",
            usage_line(
                1,
                1_755_000_000_000,
                "m1",
                Buckets {
                    input: 7,
                    output: 3,
                    cache_read: 0,
                    cache_write: 0,
                    reasoning: 0,
                },
            )
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        std::fs::write(&path, content).unwrap();
        let entries = parse(&path).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].input_tokens, 7);
    }

    #[test]
    fn skips_entries_without_billable_usage_or_model() {
        let no_usage = r#"{"type":"assistant/message","seq":1,"time":1755000000000,"data":{"turn":1,"step":1,"message":{"id":"m","role":"assistant","content":[],"source":{"kind":"model","provider":"p","model":"m1"}}}}"#;
        let zero_usage = r#"{"type":"assistant/message","seq":2,"time":1755000000000,"data":{"turn":1,"step":1,"message":{"id":"m","role":"assistant","content":[],"source":{"kind":"model","provider":"p","model":"m1"}},"usage":{"inputTokens":0,"outputTokens":0}}}"#;
        let no_model = r#"{"type":"assistant/message","seq":3,"time":1755000000000,"data":{"turn":1,"step":1,"message":{"id":"m","role":"assistant","content":[],"source":{"kind":"model","provider":"p"}},"usage":{"inputTokens":10,"outputTokens":5}}}"#;
        let no_time = r#"{"type":"assistant/message","seq":4,"data":{"turn":1,"step":1,"message":{"id":"m","role":"assistant","content":[],"source":{"kind":"model","provider":"p","model":"m1"}},"usage":{"inputTokens":10,"outputTokens":5}}}"#;
        let content = format!("{no_usage}\n{zero_usage}\n{no_model}\n{no_time}\n");
        assert!(parse_content(&content).is_empty());
    }
}
