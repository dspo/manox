// TS Pi differential fixtures: stable artifacts captured from the TS
// implementation (see tests/fixtures/session/README.md). Each test builds the
// same input the fixture generator used and asserts byte-identical output.

use manox_harness::compaction;
use manox_harness::types::{AgentMessage, ContentBlock};

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
            stop_reason: Some(manox_harness::types::StopReason::Stop),
            usage: Box::default(),
            error_message: None,
            timestamp: chrono::Utc::now(),
        },
        AgentMessage::Assistant {
            content: vec![ContentBlock::ToolUse {
                id: "t1".into(),
                name: "Read".into(),
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
            stop_reason: Some(manox_harness::types::StopReason::ToolUse),
            usage: Box::default(),
            error_message: None,
            timestamp: chrono::Utc::now(),
        },
        AgentMessage::ToolResult {
            tool_call_id: "t1".into(),
            tool_name: "Read".into(),
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
    let fixture = include_str!("fixtures/session/compaction-serialization.txt");
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

#[test]
fn substitute_args_matches_ts_fixture() {
    let fixture = include_str!("fixtures/session/substitute-args.txt");
    // Each case block records input/args/expected on separate lines; args is
    // a JSON string array. The Rust port must reproduce every case the TS
    // implementation produced.
    for block in fixture.trim().split("\n\n") {
        let mut lines = block.lines();
        // input/expected are JSON strings so values that end in whitespace
        // survive `git diff --check`; args is a JSON string array.
        let input: String =
            serde_json::from_str(lines.next().unwrap().strip_prefix("input: ").unwrap()).unwrap();
        let args: Vec<String> =
            serde_json::from_str(lines.next().unwrap().strip_prefix("args: ").unwrap()).unwrap();
        let expected: String =
            serde_json::from_str(lines.next().unwrap().strip_prefix("expected: ").unwrap())
                .unwrap();
        let rendered = manox_harness::harness::substitute_args(&input, &args);
        assert_eq!(rendered, expected, "input: {input:?}, args: {args:?}");
    }
}

#[test]
fn skill_invocation_matches_ts_fixture() {
    let fixture = include_str!("fixtures/session/skill-invocation.txt");
    let skill = manox_harness::harness::Skill {
        name: "review".into(),
        description: String::new(),
        location: "/proj/skills/review.md".into(),
        content: "Check the diff carefully.".into(),
    };
    let rendered = manox_harness::harness::format_skill_invocation(&skill, None);
    assert_eq!(rendered, fixture.trim_end());
}

#[test]
fn skill_invocation_with_instructions_matches_ts_fixture() {
    let fixture = include_str!("fixtures/session/skill-invocation-with-instructions.txt");
    let skill = manox_harness::harness::Skill {
        name: "review".into(),
        description: String::new(),
        location: "/proj/skills/review.md".into(),
        content: "Check the diff carefully.".into(),
    };
    let rendered =
        manox_harness::harness::format_skill_invocation(&skill, Some("Focus on the diff."));
    assert_eq!(rendered, fixture.trim_end());
}

#[tokio::test]
async fn single_turn_event_trace_matches_ts() {
    // The TS agent loop emits this exact lifecycle for a plain single-turn
    // run with no tools; the Rust loop must reproduce it.
    let expected: Vec<&str> = include_str!("fixtures/session/agent-loop-events.txt")
        .lines()
        .collect();
    let events = capture_single_turn_events().await;
    let kinds: Vec<String> = events
        .iter()
        .map(|e| match e {
            manox_harness::AgentEvent::AgentStart => "AgentStart".into(),
            manox_harness::AgentEvent::TurnStart => "TurnStart".into(),
            manox_harness::AgentEvent::MessageStart { message } => {
                if matches!(message.as_ref(), manox_harness::AgentMessage::User { .. }) {
                    "MessageStart(user)".into()
                } else {
                    "MessageStart(assistant)".into()
                }
            }
            manox_harness::AgentEvent::MessageEnd { message } => {
                if matches!(message.as_ref(), manox_harness::AgentMessage::User { .. }) {
                    "MessageEnd(user)".into()
                } else {
                    "MessageEnd(assistant)".into()
                }
            }
            manox_harness::AgentEvent::TurnEnd { .. } => "TurnEnd".into(),
            manox_harness::AgentEvent::AgentEnd { .. } => "AgentEnd".into(),
            other => format!("{other:?}"),
        })
        .filter(|k| !k.starts_with("Tool") && k != "AgentEnd-extra")
        .collect();
    assert_eq!(kinds, expected);
}

/// The fixed abandoned branch the TS fixture captures: user/assistant
/// messages (including tool calls and an excluded tool result), a custom
/// message, a harness-authored branch summary seeding file lists, and a
/// compaction carrier.
fn fixture_branch_entries() -> Vec<manox_harness::session::SessionTreeEntry> {
    use manox_harness::session::SessionTreeEntry;
    use manox_harness::types::ContentBlock;
    fn user(id: &str, parent: Option<&str>, text: &str) -> SessionTreeEntry {
        SessionTreeEntry::Message {
            id: id.into(),
            parent_id: parent.map(Into::into),
            timestamp: chrono::Utc::now(),
            message: manox_harness::AgentMessage::user(text),
        }
    }
    fn assistant_tool(id: &str, parent: &str, name: &str, path: &str) -> SessionTreeEntry {
        SessionTreeEntry::Message {
            id: id.into(),
            parent_id: Some(parent.into()),
            timestamp: chrono::Utc::now(),
            message: manox_harness::AgentMessage::Assistant {
                content: vec![ContentBlock::ToolUse {
                    id: "t".into(),
                    name: name.into(),
                    input: serde_json::json!({ "path": path }),
                    thought_signature: None,
                }],
                model: "m".into(),
                provider: "p".into(),
                api: "test".into(),
                response_model: None,
                response_id: None,
                diagnostics: None,
                raw_stop_reason: None,
                stop_reason: Some(manox_harness::types::StopReason::ToolUse),
                usage: Box::default(),
                error_message: None,
                timestamp: chrono::Utc::now(),
            },
        }
    }
    let tool_result = SessionTreeEntry::Message {
        id: "tr".into(),
        parent_id: Some("a3".into()),
        timestamp: chrono::Utc::now(),
        message: manox_harness::AgentMessage::ToolResult {
            tool_call_id: "t1".into(),
            tool_name: "Write".into(),
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
    };
    let custom = SessionTreeEntry::CustomMessage {
        id: "c1".into(),
        parent_id: Some("tr".into()),
        timestamp: chrono::Utc::now(),
        custom_type: "note".into(),
        content: vec![ContentBlock::Text {
            text: "custom note".into(),
            signature: None,
        }],
        details: None,
        display: true,
    };
    let branch_summary = SessionTreeEntry::BranchSummary {
        id: "bs1".into(),
        parent_id: Some("c1".into()),
        timestamp: chrono::Utc::now(),
        from_id: "u1".into(),
        summary: "prior branch".into(),
        details: Some(serde_json::json!({
            "readFiles": ["old.rs"],
            "modifiedFiles": ["old.rs"],
        })),
        usage: None,
        from_hook: Some(false),
    };
    let compaction = SessionTreeEntry::Compaction {
        id: "cp1".into(),
        parent_id: Some("bs1".into()),
        timestamp: chrono::Utc::now(),
        summary: "prior compaction".into(),
        first_kept_entry_id: None,
        tokens_before: 100,
        retained_tail: None,
        usage: None,
        details: None,
        from_hook: Some(false),
    };
    vec![
        user("u1", None, "first"),
        SessionTreeEntry::Message {
            id: "a1".into(),
            parent_id: Some("u1".into()),
            timestamp: chrono::Utc::now(),
            message: manox_harness::AgentMessage::Assistant {
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
                stop_reason: Some(manox_harness::types::StopReason::Stop),
                usage: Box::default(),
                error_message: None,
                timestamp: chrono::Utc::now(),
            },
        },
        assistant_tool("a2", "a1", "Write", "a.rs"),
        assistant_tool("a3", "a2", "Read", "b.rs"),
        tool_result,
        custom,
        branch_summary,
        compaction,
    ]
}

#[test]
fn branch_summary_preparation_matches_ts_fixture() {
    let fixture = include_str!("fixtures/session/branch-summary-preparation.txt");
    let mut lines = fixture.lines();
    let roles: Vec<String> =
        serde_json::from_str(lines.next().unwrap().strip_prefix("roles: ").unwrap()).unwrap();
    let read_files: Vec<String> =
        serde_json::from_str(lines.next().unwrap().strip_prefix("readFiles: ").unwrap()).unwrap();
    let modified_files: Vec<String> = serde_json::from_str(
        lines
            .next()
            .unwrap()
            .strip_prefix("modifiedFiles: ")
            .unwrap(),
    )
    .unwrap();
    assert_eq!(lines.next().unwrap(), "conversation:");
    let expected_conversation = lines.collect::<Vec<_>>().join("\n");

    let entries = fixture_branch_entries();
    let prep = manox_harness::compaction::branch_summarization::prepare_branch_entries(&entries, 0);
    let actual_roles: Vec<String> = prep.messages.iter().map(role_name).collect();
    assert_eq!(actual_roles, roles, "prepared message roles");
    assert_eq!(prep.read_files, read_files);
    assert_eq!(prep.modified_files, modified_files);
    let conversation = manox_harness::compaction::serialize_conversation(&prep.messages);
    assert_eq!(
        conversation, expected_conversation,
        "serialized conversation"
    );
}

/// The fixture role label of a prepared message: the tagged carriers identify
/// branch/compaction summaries by their prefix.
fn role_name(message: &manox_harness::AgentMessage) -> String {
    match message {
        manox_harness::AgentMessage::User { content, .. } => {
            let text = content
                .iter()
                .find_map(|b| match b {
                    manox_harness::types::ContentBlock::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .unwrap_or("");
            if text.starts_with(manox_harness::session::BRANCH_SUMMARY_PREFIX) {
                "branchSummary".into()
            } else if text.starts_with(manox_harness::session::COMPACTION_SUMMARY_PREFIX) {
                "compactionSummary".into()
            } else {
                "user".into()
            }
        }
        manox_harness::AgentMessage::Assistant { .. } => "assistant".into(),
        manox_harness::AgentMessage::Custom { .. } => "custom".into(),
        manox_harness::AgentMessage::BashExecution { .. } => "bashExecution".into(),
        manox_harness::AgentMessage::ToolResult { .. } => "toolResult".into(),
    }
}

/// Run one plain turn through the loop and capture the emitted lifecycle.
async fn capture_single_turn_events() -> Vec<manox_harness::AgentEvent> {
    use async_trait::async_trait;
    use manox_harness::agent_loop::{StreamFn, run_loop};
    use manox_harness::types::AgentLoopConfig;
    use tokio_util::sync::CancellationToken;

    struct LocalSink(std::sync::Arc<std::sync::Mutex<Vec<manox_harness::AgentEvent>>>);
    #[async_trait]
    impl manox_harness::EventSink for LocalSink {
        async fn emit(&self, event: manox_harness::AgentEvent) -> Result<(), anyhow::Error> {
            self.0.lock().unwrap().push(event);
            Ok(())
        }
    }

    struct ToolCtx;
    #[async_trait]
    impl manox_harness::tool::ToolContext for ToolCtx {
        fn env(&self) -> &dyn manox_harness::env::ExecutionEnv {
            panic!("no tools in this trace")
        }
        fn cwd(&self) -> &std::path::Path {
            panic!("no tools in this trace")
        }
        fn tool_state(&self) -> &manox_harness::tool::ToolState {
            static STATE: std::sync::OnceLock<manox_harness::tool::ToolState> =
                std::sync::OnceLock::new();
            STATE.get_or_init(manox_harness::tool::ToolState::new)
        }
    }

    struct OneShot;
    #[async_trait]
    impl StreamFn for OneShot {
        async fn stream(
            &self,
            _c: &manox_harness::AgentContext,
            _s: CancellationToken,
            _tx: tokio::sync::mpsc::Sender<manox_harness::AgentEvent>,
        ) -> Result<manox_harness::AgentMessage, anyhow::Error> {
            Ok(manox_harness::AgentMessage::Assistant {
                content: vec![manox_harness::types::ContentBlock::Text {
                    text: "ok".into(),
                    signature: None,
                }],
                model: "m".into(),
                provider: "p".into(),
                api: "test".into(),
                response_model: None,
                response_id: None,
                diagnostics: None,
                raw_stop_reason: None,
                stop_reason: Some(manox_harness::types::StopReason::Stop),
                usage: Box::default(),
                error_message: None,
                timestamp: chrono::Utc::now(),
            })
        }
    }

    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = LocalSink(std::sync::Arc::clone(&events));
    let mut context = manox_harness::AgentContext {
        system_prompt: "sys".into(),
        messages: Vec::new(),
        tools: std::sync::Arc::from(
            Vec::<std::sync::Arc<dyn manox_harness::tool::AgentTool>>::new(),
        ),
        model: manox_harness::types::Model {
            provider: "p".into(),
            api: "test".into(),
            id: "m".into(),
            context_window: 100_000,
            max_tokens: 8_192,
            thinking: manox_harness::types::ThinkingKind::None,
            metadata: Default::default(),
        },
        thinking_level: None,
        cache_retention: Default::default(),
        session_id: None,
        metadata: Default::default(),
        stream_options: Default::default(),
    };
    run_loop(
        &[manox_harness::AgentMessage::user("hi")],
        &mut context,
        &AgentLoopConfig::default(),
        None,
        std::sync::Arc::new(OneShot),
        &ToolCtx,
        &sink,
    )
    .await
    .expect("run");
    events.lock().unwrap().clone()
}
