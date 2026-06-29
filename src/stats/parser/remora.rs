//! Remora session jsonl 解析。
//!
//! Remora 基于 pi 底座构建，日志存放在 `~/.remora/projects/` 下，
//! 与 pi 共享同一 session JSONL 格式（树形、camelCase usage）。
//! 解析逻辑复用 `pi::parse_with_agent`，仅 agent 标识不同。
//!
//! 口径：raw-sum（不按 message id 去重），与 pi/OMP/Claude 保持一致。

pub(super) fn parse(content: &str) -> Vec<RawEntry> {
    super::pi::parse_with_agent(content, super::super::AGENT_REMORA)
}

use super::RawEntry;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_assistant_message() {
        let line = r#"{"type":"message","id":"abc","parentId":"xyz","timestamp":"2026-06-19T00:00:00.000Z","message":{"role":"assistant","provider":"dashscope","model":"qwen3.7-max","usage":{"input":832,"output":109,"cacheRead":2048,"cacheWrite":500,"totalTokens":3489}}}"#;
        let entries = parse(line);
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.agent, "remora");
        assert_eq!(e.model, "qwen3.7-max");
        assert_eq!(e.date, "2026-06-19");
        assert_eq!(e.input_tokens, 832);
        assert_eq!(e.output_tokens, 109);
        assert_eq!(e.cache_read_input_tokens, 2048);
        assert_eq!(e.cache_creation_input_tokens, 500);
        assert_eq!(e.session_id, None);
        assert_eq!(e.message_id, Some("abc".to_string()));
    }

    #[test]
    fn extracts_session_id_from_session_header() {
        let content = r#"{"type":"session","version":3,"id":"003b21eb-a30a-44ee-a50a-b74c09f90759","timestamp":"2026-06-19T16:04:52.232Z","cwd":"/Users/test/project"}
{"type":"message","id":"msg1","parentId":"xyz","timestamp":"2026-06-19T16:06:52.416Z","message":{"role":"assistant","model":"qwen3.7-max","usage":{"input":832,"output":109,"cacheRead":0,"cacheWrite":0,"totalTokens":941}}}"#;
        let entries = parse(content);
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].session_id,
            Some("003b21eb-a30a-44ee-a50a-b74c09f90759".to_string())
        );
    }

    #[test]
    fn skips_non_assistant_messages() {
        let line = r#"{"type":"message","id":"abc","timestamp":"2026-06-19T16:06:52.416Z","message":{"role":"user","content":[{"type":"text","text":"hello"}]}}"#;
        assert!(parse(line).is_empty());
    }

    #[test]
    fn skips_tool_result_messages() {
        let line = r#"{"type":"message","id":"abc","timestamp":"2026-06-19T16:06:52.416Z","message":{"role":"toolResult","content":"result"}}"#;
        assert!(parse(line).is_empty());
    }

    #[test]
    fn skips_error_with_zero_usage() {
        let line = r#"{"type":"message","id":"abc","parentId":"xyz","timestamp":"2026-06-19T16:06:52.416Z","message":{"role":"assistant","model":"qwen3.7-max","usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0}}}"#;
        assert!(parse(line).is_empty());
    }

    #[test]
    fn skips_empty_model() {
        let line = r#"{"type":"message","id":"abc","parentId":"xyz","timestamp":"2026-06-19T16:06:52.416Z","message":{"role":"assistant","model":"","usage":{"input":100,"output":50,"cacheRead":0,"cacheWrite":0}}}"#;
        assert!(parse(line).is_empty());
    }

    #[test]
    fn skips_synthetic_model() {
        let line = r#"{"type":"message","id":"abc","parentId":"xyz","timestamp":"2026-06-19T16:06:52.416Z","message":{"role":"assistant","model":"<synthetic>","usage":{"input":100,"output":50,"cacheRead":0,"cacheWrite":0}}}"#;
        assert!(parse(line).is_empty());
    }

    #[test]
    fn skips_compaction_entries() {
        let line = r#"{"type":"compaction","id":"c1","parentId":"p1","timestamp":"2026-06-19T16:06:52.416Z","summary":"...","firstKeptEntryId":"e1","tokensBefore":5000}"#;
        assert!(parse(line).is_empty());
    }

    #[test]
    fn skips_model_change_entries() {
        let line = r#"{"type":"model_change","id":"mc1","parentId":"p1","timestamp":"2026-06-19T16:04:52.234Z","provider":"dashscope","modelId":"qwen3.7-max"}"#;
        assert!(parse(line).is_empty());
    }

    #[test]
    fn later_session_header_overrides_first() {
        let content = r#"{"type":"session","version":3,"id":"sess-1","timestamp":"2026-06-19T16:00:00.000Z","cwd":"/test"}
{"type":"message","id":"m1","timestamp":"2026-06-19T12:00:00.000Z","message":{"role":"assistant","model":"qwen3.7-max","usage":{"input":100,"output":50,"cacheRead":0,"cacheWrite":0}}}
{"type":"session","version":3,"id":"sess-2","timestamp":"2026-06-19T17:00:00.000Z","cwd":"/test"}
{"type":"message","id":"m2","timestamp":"2026-06-19T12:00:00.000Z","message":{"role":"assistant","model":"qwen3.7-max","usage":{"input":200,"output":100,"cacheRead":0,"cacheWrite":0}}}"#;
        let entries = parse(content);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].session_id, Some("sess-1".to_string()));
        assert_eq!(entries[1].session_id, Some("sess-2".to_string()));
    }
}
