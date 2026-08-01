// TS Pi differential fixtures: stable artifacts captured from the TS
// implementation (see tests/fixtures/ts-pi/README.md). Each test builds the
// same input the fixture generator used and asserts byte-identical output.

use pi::compaction;
use pi::types::{AgentMessage, ContentBlock};

fn conversation() -> Vec<AgentMessage> {
    vec![
        AgentMessage::user("hello"),
        AgentMessage::Assistant {
            content: vec![ContentBlock::Text {
                text: "hi".into(),
                signature: None,
            }],
            model: "m".into(),
            provider: "p".into(),
            api: "test".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            raw_stop_reason: None,
            stop_reason: Some(pi::types::StopReason::Stop),
            usage: Box::default(),
            error_message: None,
            timestamp: chrono::Utc::now(),
        },
        AgentMessage::Assistant {
            content: vec![ContentBlock::ToolUse {
                id: "t1".into(),
                name: "read".into(),
                input: serde_json::json!({"path": "a.rs"}),
                thought_signature: None,
            }],
            model: "m".into(),
            provider: "p".into(),
            api: "test".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            raw_stop_reason: None,
            stop_reason: Some(pi::types::StopReason::ToolUse),
            usage: Box::default(),
            error_message: None,
            timestamp: chrono::Utc::now(),
        },
        AgentMessage::ToolResult {
            tool_call_id: "t1".into(),
            tool_name: "read".into(),
            content: vec![ContentBlock::Text {
                text: "ok".into(),
                signature: None,
            }],
            is_error: false,
            details: None,
            usage: None,
            added_tool_names: None,
            timestamp: chrono::Utc::now(),
        },
    ]
}

#[test]
fn serialize_conversation_matches_ts_fixture() {
    let fixture = include_str!("fixtures/ts-pi/compaction-serialization.txt");
    let rendered = compaction::serialize_conversation(&conversation());
    assert_eq!(rendered, fixture.trim_end());
}

#[test]
fn summarization_system_prompt_matches_ts() {
    // The TS prompt text is a stable artifact; Rust embeds it verbatim.
    assert!(
        compaction::SUMMARIZATION_SYSTEM_PROMPT
            .starts_with("You are a context summarization assistant.")
    );
    assert!(compaction::SUMMARIZATION_SYSTEM_PROMPT.contains("Do NOT continue the conversation."));
}

#[test]
fn file_operation_formatting_matches_ts() {
    // TS formatFileOperations: read-only and modified lists, empty input
    // yields nothing.
    assert_eq!(compaction::format_file_operations(&[], &[]), "");
    let formatted =
        compaction::format_file_operations(&["a.rs".to_string()], &["b.rs".to_string()]);
    assert!(formatted.contains("<read-files>\na.rs\n</read-files>"));
    assert!(formatted.contains("<modified-files>\nb.rs\n</modified-files>"));
    assert!(formatted.starts_with("\n\n"));
}
