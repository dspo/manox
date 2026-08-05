//! Pure mappings between the pi harness wire types and the UI's language.
//!
//! The workspace renders two data shapes: the `ThreadEvent` stream (live
//! deltas) and `agent::Message` history (rebuild). A pi `AgentSession`
//! produces `AgentEvent`s and `AgentMessage`s; the functions here translate
//! them into those two shapes so the polished manox render pipeline
//! (`ConversationState::apply` / `build_items`) is reused untouched.
//!
//! Both directions are pure functions — no gpui, no IO — so they carry their
//! own unit tests.

use agent::language_model::{
    LanguageModelToolResult, LanguageModelToolUse, MessageContent, StopReason as ManoxStopReason,
};
use agent::thread::ToolCallStatus;
use agent::{Message, ThreadEvent};
use pi::tool::AgentToolResult;
use pi::types::{
    AgentEvent, AgentMessage, AssistantMessageEvent, ContentBlock, StopReason as PiStopReason,
};

/// Map one pi `AgentEvent` onto the `ThreadEvent`s the workspace renders.
///
/// Events with no UI counterpart (run/turn lifecycle handled by the session
/// actor, message boundaries, block start/end markers) map to nothing. The
/// manox-only variants (`ToolCallAuthorization`, `Plan*`, sub-agent events)
/// are never produced — approval and plan flows are not wired in this stage.
pub fn agent_event_to_thread_events(event: &AgentEvent) -> Vec<ThreadEvent> {
    match event {
        AgentEvent::AgentStart | AgentEvent::AgentEnd { .. } => Vec::new(),
        // `TurnStarted` is emitted once by `PiSession::run_turn` (matching
        // `Thread` semantics); pi's per-round `TurnStart` must not duplicate it.
        AgentEvent::TurnStart => Vec::new(),
        AgentEvent::MessageStart { .. } => Vec::new(),
        AgentEvent::MessageUpdate {
            assistant_message_event,
            ..
        } => match assistant_message_event {
            AssistantMessageEvent::TextDelta { delta, .. } => {
                vec![ThreadEvent::AgentText(delta.clone())]
            }
            AssistantMessageEvent::ThinkingDelta { delta, .. } => {
                vec![ThreadEvent::AgentThinking(delta.clone())]
            }
            _ => Vec::new(),
        },
        AgentEvent::MessageEnd { message } => match message_stop_reason(message) {
            Some(PiStopReason::Stop) => vec![ThreadEvent::Stop(ManoxStopReason::EndTurn)],
            Some(PiStopReason::Length) => vec![ThreadEvent::Stop(ManoxStopReason::MaxTokens)],
            Some(PiStopReason::ToolUse) => vec![ThreadEvent::Stop(ManoxStopReason::ToolUse)],
            Some(PiStopReason::Aborted) => vec![ThreadEvent::Stop(ManoxStopReason::Cancelled)],
            Some(PiStopReason::Error) => {
                vec![ThreadEvent::Error(anyhow::anyhow!(
                    "{}",
                    message_error_text(message)
                ))]
            }
            None => Vec::new(),
        },
        AgentEvent::ToolExecutionStart {
            tool_call_id,
            tool_name,
            arguments,
        } => vec![ThreadEvent::ToolCall {
            id: tool_call_id.clone(),
            name: tool_name.clone(),
            title: tool_title(tool_name, arguments),
            status: ToolCallStatus::Running,
            input: Some(arguments.clone()),
        }],
        AgentEvent::ToolExecutionUpdate {
            tool_call_id,
            partial_result,
            ..
        } => {
            // The pi-extensions bash tool streams `{"output": chunk}` partials;
            // surface them as live tool output. Other partial shapes carry no
            // renderable text.
            match partial_result.get("output").and_then(|v| v.as_str()) {
                Some(chunk) if !chunk.is_empty() => vec![ThreadEvent::ToolOutput {
                    id: tool_call_id.clone(),
                    chunk: chunk.to_string(),
                }],
                _ => Vec::new(),
            }
        }
        AgentEvent::ToolExecutionEnd {
            tool_call_id,
            tool_name,
            result,
            is_error,
        } => vec![
            ThreadEvent::ToolCall {
                id: tool_call_id.clone(),
                name: tool_name.clone(),
                title: tool_name.clone(),
                status: if *is_error {
                    ToolCallStatus::Error
                } else {
                    ToolCallStatus::Success
                },
                input: None,
            },
            ThreadEvent::ToolResult {
                id: tool_call_id.clone(),
                output: tool_result_text(result),
                is_error: *is_error,
            },
        ],
        AgentEvent::Retry {
            attempt,
            max_attempts,
            delay,
            reason,
            detail,
        } => vec![ThreadEvent::Retry {
            attempt: *attempt,
            max_attempts: *max_attempts,
            delay_secs: delay.as_secs(),
            reason: reason.clone(),
            detail: detail.clone(),
        }],
        // Turn boundaries are owned by the session actor, which knows whether
        // the run was cancelled or failed and which steers stranded.
        AgentEvent::TurnEnd { .. } => Vec::new(),
    }
}

/// Restore mapping: pi harness history onto the `agent::Message` history the
/// rebuild path (`build_items`) renders. Blocks map one-to-one; terminal
/// error/abort states surface as a trailing assistant text note so a reloaded
/// session shows why the last run stopped.
pub fn harness_messages_to_messages(input: &[AgentMessage]) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::new();
    for m in input {
        match m {
            AgentMessage::User { content, .. } => {
                let content: Vec<MessageContent> = content
                    .iter()
                    .map(content_block_to_message_content)
                    .collect();
                out.push(Message::user_with_content(content));
            }
            AgentMessage::Assistant {
                content,
                stop_reason,
                error_message,
                ..
            } => {
                let mut blocks: Vec<MessageContent> = content
                    .iter()
                    .map(content_block_to_message_content)
                    .collect();
                if matches!(stop_reason, Some(PiStopReason::Error)) {
                    blocks.push(MessageContent::Text(format!(
                        "[turn failed: {}]",
                        error_message.as_deref().unwrap_or("unknown error")
                    )));
                }
                if matches!(stop_reason, Some(PiStopReason::Aborted)) {
                    blocks.push(MessageContent::Text("[turn aborted]".to_string()));
                }
                out.push(Message::assistant(blocks));
            }
            AgentMessage::BashExecution {
                command, output, ..
            } => {
                // TODO(pi-wire): dedicated shell-record card; render inline for now.
                out.push(Message::assistant(vec![MessageContent::Text(format!(
                    "$ {command}\n{output}"
                ))]));
            }
            AgentMessage::Custom {
                content, display, ..
            } => {
                if *display {
                    let blocks: Vec<MessageContent> = content
                        .iter()
                        .map(content_block_to_message_content)
                        .collect();
                    if !blocks.is_empty() {
                        out.push(Message::assistant(blocks));
                    }
                }
            }
            AgentMessage::ToolResult {
                tool_call_id,
                tool_name,
                content,
                is_error,
                ..
            } => {
                // Tool results live in `Role::User` messages per the wire
                // contract `build_items` expects.
                out.push(Message::user_with_content(vec![
                    MessageContent::ToolResult(LanguageModelToolResult {
                        tool_use_id: tool_call_id.clone(),
                        tool_name: tool_name.clone().into(),
                        is_error: *is_error,
                        content: text_of_blocks(content),
                    }),
                ]));
            }
        }
    }
    out
}

/// One pi content block onto one manox content block.
fn content_block_to_message_content(block: &ContentBlock) -> MessageContent {
    match block {
        ContentBlock::Text { text, .. } => MessageContent::Text(text.clone()),
        ContentBlock::Thinking {
            thinking,
            signature,
            ..
        } => MessageContent::Thinking {
            text: thinking.clone(),
            signature: signature.clone(),
        },
        ContentBlock::Image { data, mime_type } => MessageContent::Image {
            data: data.clone(),
            mime_type: mime_type.clone(),
        },
        ContentBlock::ToolUse {
            id,
            name,
            input,
            thought_signature,
        } => MessageContent::ToolUse(LanguageModelToolUse {
            id: id.clone(),
            name: name.clone().into(),
            raw_input: input.to_string(),
            input: input.clone(),
            is_input_complete: true,
            thought_signature: thought_signature.clone(),
        }),
    }
}

/// Concatenate the text blocks of a tool result for display.
fn tool_result_text(result: &AgentToolResult) -> String {
    text_of_blocks(&result.content)
}

fn text_of_blocks(blocks: &[ContentBlock]) -> String {
    let mut out = String::new();
    for block in blocks {
        if let ContentBlock::Text { text, .. } = block {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(text);
        }
    }
    out
}

fn message_stop_reason(message: &AgentMessage) -> Option<PiStopReason> {
    match message {
        AgentMessage::Assistant { stop_reason, .. } => *stop_reason,
        _ => None,
    }
}

fn message_error_text(message: &AgentMessage) -> String {
    match message {
        AgentMessage::Assistant { error_message, .. } => error_message
            .clone()
            .unwrap_or_else(|| "the pi session hit an error".to_string()),
        _ => "the pi session hit an error".to_string(),
    }
}

/// Human-readable tool card title from the pi tool name + arguments. Falls
/// back to the bare name for tools without a recognized target field.
pub fn tool_title(name: &str, args: &serde_json::Value) -> String {
    let arg = |key: &str| -> Option<String> {
        args.get(key)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    match name {
        "read" | "write" | "ls" => match arg("path") {
            Some(path) => format!("{name} {path}"),
            None => name.to_string(),
        },
        "edit" | "edit_diff" => match arg("path") {
            Some(path) => format!("edit {path}"),
            None => "edit".to_string(),
        },
        "grep" => match (arg("pattern"), arg("path")) {
            (Some(pattern), Some(path)) => format!("grep {pattern} {path}"),
            (Some(pattern), None) => format!("grep {pattern}"),
            _ => "grep".to_string(),
        },
        "find" => match arg("pattern") {
            Some(pattern) => format!("find {pattern}"),
            None => "find".to_string(),
        },
        "bash" => match arg("command") {
            Some(command) => format!("$ {command}"),
            None => "bash".to_string(),
        },
        "bash_output" => "bash output".to_string(),
        "task_stop" => "stop task".to_string(),
        "agent" => match arg("subagent_type") {
            Some(kind) => format!("agent {kind}"),
            None => "agent".to_string(),
        },
        _ => name.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi::types::{AgentMessage, Usage};

    fn assistant(stop_reason: Option<PiStopReason>, error: Option<&str>) -> Box<AgentMessage> {
        Box::new(AgentMessage::Assistant {
            content: Vec::new(),
            model: "mock".into(),
            provider: "mock".into(),
            api: "mock".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            stop_reason,
            raw_stop_reason: None,
            usage: Box::new(Usage::default()),
            error_message: error.map(str::to_string),
            timestamp: chrono::Utc::now(),
        })
    }

    fn user_msg(blocks: Vec<ContentBlock>) -> AgentMessage {
        AgentMessage::User {
            content: blocks,
            timestamp: chrono::Utc::now(),
        }
    }

    #[test]
    fn text_and_thinking_deltas_become_agent_events() {
        let text = agent_event_to_thread_events(&AgentEvent::MessageUpdate {
            message: assistant(None, None),
            assistant_message_event: AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "hello".into(),
            },
        });
        assert!(matches!(&text[..], [ThreadEvent::AgentText(t)] if t == "hello"));

        let thinking = agent_event_to_thread_events(&AgentEvent::MessageUpdate {
            message: assistant(None, None),
            assistant_message_event: AssistantMessageEvent::ThinkingDelta {
                content_index: 0,
                delta: "hmm".into(),
            },
        });
        assert!(matches!(&thinking[..], [ThreadEvent::AgentThinking(t)] if t == "hmm"));
    }

    #[test]
    fn message_end_maps_stop_reasons() {
        let cases = [
            (PiStopReason::Stop, ManoxStopReason::EndTurn),
            (PiStopReason::Length, ManoxStopReason::MaxTokens),
            (PiStopReason::ToolUse, ManoxStopReason::ToolUse),
            (PiStopReason::Aborted, ManoxStopReason::Cancelled),
        ];
        for (pi_reason, manox_reason) in cases {
            let events = agent_event_to_thread_events(&AgentEvent::MessageEnd {
                message: assistant(Some(pi_reason), None),
            });
            assert!(
                matches!(&events[..], [ThreadEvent::Stop(r)] if *r == manox_reason),
                "{pi_reason:?} -> {events:?}"
            );
        }
        let errored = agent_event_to_thread_events(&AgentEvent::MessageEnd {
            message: assistant(Some(PiStopReason::Error), Some("boom")),
        });
        assert!(
            matches!(&errored[..], [ThreadEvent::Error(e)] if e.to_string() == "boom"),
            "{errored:?}"
        );
        let streaming = agent_event_to_thread_events(&AgentEvent::MessageEnd {
            message: assistant(None, None),
        });
        assert!(streaming.is_empty());
    }

    #[test]
    fn tool_lifecycle_maps_to_tool_call_and_result() {
        let args = serde_json::json!({ "path": "src/main.rs" });
        let start = agent_event_to_thread_events(&AgentEvent::ToolExecutionStart {
            tool_call_id: "call_1".into(),
            tool_name: "read".into(),
            arguments: args.clone(),
        });
        assert!(matches!(
            &start[..],
            [ThreadEvent::ToolCall { id, name, title, status, input }]
                if id == "call_1" && name == "read" && title == "read src/main.rs"
                    && matches!(status, ToolCallStatus::Running) && input.as_ref() == Some(&args)
        ));

        let partial = agent_event_to_thread_events(&AgentEvent::ToolExecutionUpdate {
            tool_call_id: "call_1".into(),
            tool_name: "bash".into(),
            arguments: serde_json::Value::Null,
            partial_result: serde_json::json!({ "output": "line1\n" }),
        });
        assert!(matches!(
            &partial[..],
            [ThreadEvent::ToolOutput { id, chunk }] if id == "call_1" && chunk == "line1\n"
        ));

        let end = agent_event_to_thread_events(&AgentEvent::ToolExecutionEnd {
            tool_call_id: "call_1".into(),
            tool_name: "read".into(),
            result: AgentToolResult::text("file body"),
            is_error: false,
        });
        assert!(matches!(
            &end[..],
            [ThreadEvent::ToolCall { status: ToolCallStatus::Success, .. },
             ThreadEvent::ToolResult { id, output, is_error }]
                if id == "call_1" && output == "file body" && !is_error
        ));

        let failed = agent_event_to_thread_events(&AgentEvent::ToolExecutionEnd {
            tool_call_id: "call_2".into(),
            tool_name: "bash".into(),
            result: AgentToolResult {
                content: vec![ContentBlock::Text {
                    text: "exit 1".into(),
                    signature: None,
                }],
                details: None,
                is_error: true,
                usage: None,
                added_tool_names: None,
                terminate: false,
            },
            is_error: true,
        });
        assert!(matches!(
            &failed[..],
            [
                ThreadEvent::ToolCall {
                    status: ToolCallStatus::Error,
                    ..
                },
                ThreadEvent::ToolResult { is_error: true, .. }
            ]
        ));
    }

    #[test]
    fn retry_maps_through() {
        let events = agent_event_to_thread_events(&AgentEvent::Retry {
            attempt: 1,
            max_attempts: 3,
            delay: std::time::Duration::from_secs(2),
            reason: "429 Too Many Requests".into(),
            detail: Some("slow down".into()),
        });
        assert!(matches!(
            &events[..],
            [ThreadEvent::Retry { attempt: 1, max_attempts: 3, delay_secs: 2, reason, detail: Some(d) }]
                if reason == "429 Too Many Requests" && d == "slow down"
        ));
    }

    #[test]
    fn lifecycle_events_map_to_nothing() {
        for event in [
            AgentEvent::AgentStart,
            AgentEvent::TurnStart,
            AgentEvent::AgentEnd {
                messages: Vec::new(),
            },
            AgentEvent::TurnEnd {
                message: assistant(Some(PiStopReason::Stop), None),
                tool_results: Vec::new(),
            },
        ] {
            assert!(agent_event_to_thread_events(&event).is_empty(), "{event:?}");
        }
    }

    #[test]
    fn restore_maps_user_assistant_and_tool_result_blocks() {
        let history = vec![
            user_msg(vec![ContentBlock::Text {
                text: "do it".into(),
                signature: None,
            }]),
            AgentMessage::Assistant {
                content: vec![
                    ContentBlock::Thinking {
                        thinking: "plan".into(),
                        signature: Some("sig".into()),
                        redacted: Some(false),
                    },
                    ContentBlock::ToolUse {
                        id: "call_1".into(),
                        name: "read".into(),
                        input: serde_json::json!({"path": "a.rs"}),
                        thought_signature: None,
                    },
                ],
                model: "m".into(),
                provider: "p".into(),
                api: "anthropic".into(),
                response_model: None,
                response_id: None,
                diagnostics: None,
                stop_reason: Some(PiStopReason::ToolUse),
                raw_stop_reason: None,
                usage: Box::default(),
                error_message: None,
                timestamp: chrono::Utc::now(),
            },
            AgentMessage::ToolResult {
                tool_call_id: "call_1".into(),
                tool_name: "read".into(),
                content: vec![ContentBlock::Text {
                    text: "body".into(),
                    signature: None,
                }],
                is_error: false,
                details: None,
                usage: None,
                added_tool_names: None,
                timestamp: chrono::Utc::now(),
            },
        ];

        let messages = harness_messages_to_messages(&history);
        assert_eq!(messages.len(), 3);
        assert!(matches!(
            messages[0].role,
            agent::language_model::Role::User
        ));
        assert!(matches!(
            messages[0].content.as_slice(),
            [MessageContent::Text(t)] if t == "do it"
        ));
        assert!(matches!(
            messages[1].content.as_slice(),
            [MessageContent::Thinking { text, signature: Some(sig) },
             MessageContent::ToolUse(use_)]
                if text == "plan" && sig == "sig" && use_.id == "call_1"
                    && use_.name.as_ref() == "read" && use_.is_input_complete
        ));
        // Tool result lands in a user-role message per the wire contract.
        assert!(matches!(
            messages[2].role,
            agent::language_model::Role::User
        ));
        assert!(matches!(
            messages[2].content.as_slice(),
            [MessageContent::ToolResult(r)]
                if r.tool_use_id == "call_1" && r.content == "body" && !r.is_error
        ));
    }

    #[test]
    fn restore_surfaces_error_and_aborted_terminal_states() {
        let mut failed = assistant(Some(PiStopReason::Error), Some("provider 500"));
        if let AgentMessage::Assistant { content, .. } = &mut *failed {
            content.push(ContentBlock::Text {
                text: "partial".into(),
                signature: None,
            });
        }
        let aborted = assistant(Some(PiStopReason::Aborted), None);

        let messages = harness_messages_to_messages(&[
            user_msg(vec![ContentBlock::Text {
                text: "go".into(),
                signature: None,
            }]),
            *failed,
            *aborted,
        ]);
        assert_eq!(messages.len(), 3);
        assert!(matches!(
            messages[1].content.as_slice(),
            [MessageContent::Text(t), MessageContent::Text(note)]
                if t == "partial" && note == "[turn failed: provider 500]"
        ));
        assert!(matches!(
            messages[2].content.as_slice(),
            [MessageContent::Text(note)] if note == "[turn aborted]"
        ));
    }

    #[test]
    fn tool_titles_use_the_recognized_target_field() {
        assert_eq!(
            tool_title("bash", &serde_json::json!({"command": "ls -la"})),
            "$ ls -la"
        );
        assert_eq!(
            tool_title("edit", &serde_json::json!({"path": "a.rs"})),
            "edit a.rs"
        );
        assert_eq!(
            tool_title(
                "agent",
                &serde_json::json!({"subagent_type": "Explore", "prompt": "x"})
            ),
            "agent Explore"
        );
        assert_eq!(tool_title("custom", &serde_json::json!({})), "custom");
    }
}
