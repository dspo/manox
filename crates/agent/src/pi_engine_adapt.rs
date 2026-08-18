use super::*;
use crate::language_model::{
    LanguageModelToolResult, LanguageModelToolUse, StopReason as ManoxStopReason,
};
use crate::thread::ToolCallStatus;
use pi::types::StopReason as PiStopReason;

/// Map one pi `AgentEvent` onto the `ThreadEvent`s the workspace renders.
///
/// Events with no UI counterpart (run/turn lifecycle handled by the facade,
/// message boundaries, block start/end markers) map to nothing.
/// `ToolCallAuthorization` never comes from this mapping — the approval
/// gate (`pi_approval`) emits it directly while parked on a verdict.
/// `Plan*` and sub-agent events remain manox-only and are never produced.
pub fn agent_event_to_thread_events(event: &AgentEvent) -> Vec<ThreadEvent> {
    match event {
        AgentEvent::AgentStart | AgentEvent::AgentEnd { .. } => Vec::new(),
        // `TurnStarted` is emitted once by the facade (matching `Thread`
        // semantics); pi's per-round `TurnStart` must not duplicate it.
        AgentEvent::TurnStart => Vec::new(),
        AgentEvent::MessageStart { .. } => Vec::new(),
        AgentEvent::MessageUpdate {
            assistant_message_event,
            ..
        } => match assistant_message_event {
            pi::types::AssistantMessageEvent::TextDelta { delta, .. } => {
                vec![ThreadEvent::AgentText(delta.clone())]
            }
            pi::types::AssistantMessageEvent::ThinkingDelta { delta, .. } => {
                vec![ThreadEvent::AgentThinking(delta.clone())]
            }
            _ => Vec::new(),
        },
        AgentEvent::MessageEnd { message } => match message_stop_reason(message) {
            Some(PiStopReason::Stop) => vec![ThreadEvent::Stop(ManoxStopReason::EndTurn)],
            Some(PiStopReason::Length) => vec![ThreadEvent::Stop(ManoxStopReason::MaxTokens)],
            Some(PiStopReason::ToolUse) => vec![ThreadEvent::Stop(ManoxStopReason::ToolUse)],
            Some(PiStopReason::Aborted) => {
                vec![ThreadEvent::Stop(ManoxStopReason::Cancelled)]
            }
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
        } => {
            let mut events = vec![ThreadEvent::ToolCall {
                id: tool_call_id.clone(),
                name: tool_name.clone(),
                title: tool_title(tool_name, arguments),
                status: ToolCallStatus::Running,
                input: Some(arguments.clone()),
            }];
            // A spawned sub-agent also lands as a rail observation row
            // (the conversation shows the Agent tool call card; the rail
            // tracks the nested session's lifecycle).
            if tool_name == crate::tools::AGENT {
                events.push(ThreadEvent::SubagentProgress {
                    id: tool_call_id.clone(),
                    subagent_type: arguments
                        .get("subagent_type")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    tool_uses: 0,
                    token_usage: crate::language_model::TokenUsage::default(),
                    latest_activity: arguments
                        .get("prompt")
                        .and_then(|v| v.as_str())
                        .map(crate::tools::subagent_topic),
                    status: ToolCallStatus::Running,
                });
            }
            events
        }
        AgentEvent::ToolExecutionUpdate {
            tool_call_id,
            tool_name,
            partial_result,
            ..
        } => {
            // The Agent tool bridges its child session's streamed events
            // as `{"subagent_event": {...}}` progress: surface them as
            // drill-down transcript events + live rail activity.
            if tool_name == crate::tools::AGENT
                && let Some(ev) = partial_result.get("subagent_event")
            {
                let mut events = Vec::new();
                let kind = ev.get("kind").and_then(|v| v.as_str()).unwrap_or_default();
                let activity = |text: String| ThreadEvent::SubagentProgress {
                    id: tool_call_id.clone(),
                    subagent_type: String::new(),
                    tool_uses: 0,
                    token_usage: crate::language_model::TokenUsage::default(),
                    latest_activity: Some(text),
                    status: ToolCallStatus::Running,
                };
                match kind {
                    "text" => {
                        if let Some(text) = ev.get("text").and_then(|v| v.as_str()) {
                            events.push(ThreadEvent::SubagentChild {
                                id: tool_call_id.clone(),
                                child: crate::thread::SubagentChildEvent::Text(text.to_string()),
                            });
                        }
                    }
                    "thinking" => {
                        if let Some(text) = ev.get("text").and_then(|v| v.as_str()) {
                            events.push(ThreadEvent::SubagentChild {
                                id: tool_call_id.clone(),
                                child: crate::thread::SubagentChildEvent::Thinking(
                                    text.to_string(),
                                ),
                            });
                        }
                    }
                    "tool_start" => {
                        let name = ev.get("tool").and_then(|v| v.as_str()).unwrap_or_default();
                        let child_id = ev
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string();
                        let hint = ev
                            .get("summary_key")
                            .and_then(|v| v.as_str())
                            .map(|k| k.to_string())
                            .zip(
                                ev.get("summary")
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.to_string()),
                            );
                        events.push(ThreadEvent::SubagentChild {
                            id: tool_call_id.clone(),
                            child: crate::thread::SubagentChildEvent::ToolStart {
                                id: child_id,
                                name: name.to_string(),
                                hint: hint.clone(),
                            },
                        });
                        events.push(activity(match hint {
                            Some((_, s)) => format!("▸ {name} {s}"),
                            None => format!("▸ {name}"),
                        }));
                    }
                    "tool_end" => {
                        let name = ev.get("tool").and_then(|v| v.as_str()).unwrap_or_default();
                        let child_id = ev
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string();
                        let is_error = ev
                            .get("is_error")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        events.push(ThreadEvent::SubagentChild {
                            id: tool_call_id.clone(),
                            child: crate::thread::SubagentChildEvent::ToolEnd {
                                id: child_id,
                                name: name.to_string(),
                                is_error,
                            },
                        });
                        events.push(activity(format!(
                            "{} {name}",
                            if is_error { "✗" } else { "✓" }
                        )));
                    }
                    _ => {}
                }
                return events;
            }
            // The pi-extensions bash tool streams `{"output": chunk}`
            // partials; surface them as live tool output. Other partial
            // shapes carry no renderable text.
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
        } => {
            let status = if *is_error {
                ToolCallStatus::Error
            } else {
                ToolCallStatus::Success
            };
            let mut events = vec![
                ThreadEvent::ToolCall {
                    id: tool_call_id.clone(),
                    name: tool_name.clone(),
                    title: tool_name.clone(),
                    status,
                    input: None,
                },
                ThreadEvent::ToolResult {
                    id: tool_call_id.clone(),
                    output: tool_result_text(result),
                    is_error: *is_error,
                },
            ];
            // Close the sub-agent's rail observation row (the row itself
            // was created by the start event; empty type here is fine —
            // the upsert keeps the existing entry's fields).
            if tool_name == crate::tools::AGENT {
                events.push(ThreadEvent::SubagentProgress {
                    id: tool_call_id.clone(),
                    subagent_type: String::new(),
                    tool_uses: 0,
                    token_usage: crate::language_model::TokenUsage::default(),
                    latest_activity: None,
                    status,
                });
            }
            events
        }
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
        // Turn boundaries are owned by the facade, which knows whether the
        // run was cancelled or failed and which steers stranded.
        AgentEvent::TurnEnd { .. } => Vec::new(),
    }
}

/// Restore mapping: pi harness history onto the `agent::Message` history
/// the rebuild path (`build_items`) renders. Blocks map one-to-one;
/// terminal error/abort states surface as a trailing assistant text note
/// so a reloaded session shows why the last run stopped.
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
                // Plan blocks surface through the PlanReady review flow;
                // strip them from the displayed transcript (the session
                // jsonl keeps the raw text, manox parity).
                for block in blocks.iter_mut() {
                    if let MessageContent::Text(text) = block {
                        *text = crate::proposed_plan::strip_proposed_plan_blocks(text);
                    }
                }
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
                // Dedicated shell-record card comes later; inline for now.
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
fn tool_result_text(result: &pi::tool::AgentToolResult) -> String {
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

/// Human-readable tool card title from the pi tool name + arguments.
/// Falls back to the bare name for tools without a recognized target
/// field.
pub fn tool_title(name: &str, args: &serde_json::Value) -> String {
    let arg = |key: &str| -> Option<String> {
        args.get(key)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    match name {
        "Read" | "Write" | "Ls" => match arg("path") {
            Some(path) => format!("{name} {path}"),
            None => name.to_string(),
        },
        "Edit" | "EditDiff" => match arg("path") {
            Some(path) => format!("Edit {path}"),
            None => "Edit".to_string(),
        },
        "Grep" => match (arg("pattern"), arg("path")) {
            (Some(pattern), Some(path)) => format!("Grep {pattern} {path}"),
            (Some(pattern), None) => format!("Grep {pattern}"),
            _ => "Grep".to_string(),
        },
        "Find" => match arg("pattern") {
            Some(pattern) => format!("Find {pattern}"),
            None => "Find".to_string(),
        },
        "Bash" => match arg("command") {
            Some(command) => format!("$ {command}"),
            None => "Bash".to_string(),
        },
        "BashOutput" => "BashOutput".to_string(),
        "TaskStop" => "TaskStop".to_string(),
        "Agent" => match arg("subagent_type") {
            Some(kind) => format!("Agent {kind}"),
            None => "Agent".to_string(),
        },
        _ => name.to_string(),
    }
}
