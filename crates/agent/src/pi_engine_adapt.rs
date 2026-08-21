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

/// Project the session entry list onto the display sequence the UI mirror
/// and the conversation rebuild share: every projected context message
/// becomes a `HistoryEntry::Message`, and `manox_ui_note` custom entries
/// become `HistoryEntry::Note` at their persisted position. Returns the
/// positioned notes alongside so live mirror refreshes can re-merge them
/// (the live transcript carries no custom entries).
/// A compaction boundary carrying a materialized `retained_tail` also folds
/// away notes inside the kept segment (the tail payload holds only
/// messages) — the same loss as the summarized history itself.
pub fn entries_to_display(
    entries: &[pi::session::SessionTreeEntry],
) -> (Vec<HistoryEntry>, Vec<PositionedNote>) {
    let mut display: Vec<HistoryEntry> = Vec::new();
    let mut notes: Vec<PositionedNote> = Vec::new();
    let mut message_count = 0usize;
    for entry in entries {
        if let pi::session::SessionTreeEntry::Custom {
            custom_type, data, ..
        } = entry
        {
            if custom_type == UI_NOTE_CUSTOM_TYPE {
                match data
                    .as_ref()
                    .map(|d| serde_json::from_value::<UiNoteRecord>(d.clone()))
                {
                    Some(Ok(note)) => {
                        notes.push(PositionedNote {
                            note: note.clone(),
                            after_message: message_count,
                        });
                        display.push(HistoryEntry::Note(note));
                    }
                    Some(Err(err)) => {
                        tracing::warn!(error = %err, "unparseable UI note entry skipped")
                    }
                    None => {}
                }
            }
            continue;
        }
        for message in pi::session::session_entry_to_context_messages(entry) {
            let mapped = harness_messages_to_messages(std::slice::from_ref(&message));
            message_count += mapped.len();
            display.extend(mapped.into_iter().map(HistoryEntry::Message));
        }
    }
    (display, notes)
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
/// Browser tools (WebExplore* / ChromeUse*) surface their url, tab id, or
/// element ref so approval cards carry the decision-relevant target. Falls
/// back to the bare name for tools without a recognized target field.
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
        "Glob" => match arg("pattern") {
            Some(pattern) => format!("Glob {pattern}"),
            None => "Glob".to_string(),
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
        "WebExploreOpen" | "ChromeUseOpen" => match arg("url").or_else(|| arg("cdp_endpoint")) {
            Some(target) => format!("{name} {target}"),
            None => name.to_string(),
        },
        "WebExploreNavigate" | "ChromeUseNavigate" => match arg("url") {
            Some(url) => format!("{name} {url}"),
            None => name.to_string(),
        },
        other if other.starts_with("WebExplore") || other.starts_with("ChromeUse") => {
            let tab = args.get("tab_id").and_then(|v| v.as_u64());
            match (tab, arg("ref")) {
                (Some(tab), Some(element_ref)) => format!("{other} tab {tab} [{element_ref}]"),
                (Some(tab), None) => format!("{other} tab {tab}"),
                _ => other.to_string(),
            }
        }
        _ => name.to_string(),
    }
}

/// Map one child sub-agent `AgentEvent` onto the observation events the
/// workspace accumulates for the sub-agent panel: `SubagentChild` (transcript
/// deltas + tool lifecycle) plus a running `SubagentProgress` for rail rows.
/// Mirrors the retired JSON bridge's kind mapping but reads the child session's
/// events directly instead of a `{"subagent_event": ...}` progress payload.
pub fn child_events_of(id: &str, event: &AgentEvent) -> Vec<ThreadEvent> {
    let activity = |text: String| ThreadEvent::SubagentProgress {
        id: id.to_string(),
        subagent_type: String::new(),
        tool_uses: 0,
        token_usage: crate::language_model::TokenUsage::default(),
        latest_activity: Some(text),
        status: ToolCallStatus::Running,
    };
    match event {
        AgentEvent::MessageUpdate {
            assistant_message_event,
            ..
        } => match assistant_message_event {
            pi::types::AssistantMessageEvent::TextDelta { delta, .. } => {
                vec![ThreadEvent::SubagentChild {
                    id: id.to_string(),
                    child: crate::thread::SubagentChildEvent::Text(delta.clone()),
                }]
            }
            pi::types::AssistantMessageEvent::ThinkingDelta { delta, .. } => {
                vec![ThreadEvent::SubagentChild {
                    id: id.to_string(),
                    child: crate::thread::SubagentChildEvent::Thinking(delta.clone()),
                }]
            }
            _ => Vec::new(),
        },
        AgentEvent::ToolExecutionStart {
            tool_call_id,
            tool_name,
            arguments,
        } => {
            let hint = arg_hint(arguments);
            let mut events = vec![ThreadEvent::SubagentChild {
                id: id.to_string(),
                child: crate::thread::SubagentChildEvent::ToolStart {
                    id: tool_call_id.clone(),
                    name: tool_name.clone(),
                    hint: hint.clone(),
                },
            }];
            events.push(activity(match hint {
                Some((_, s)) => format!("▸ {tool_name} {s}"),
                None => format!("▸ {tool_name}"),
            }));
            events
        }
        AgentEvent::ToolExecutionEnd {
            tool_call_id,
            tool_name,
            is_error,
            ..
        } => vec![ThreadEvent::SubagentChild {
            id: id.to_string(),
            child: crate::thread::SubagentChildEvent::ToolEnd {
                id: tool_call_id.clone(),
                name: tool_name.clone(),
                is_error: *is_error,
            },
        }],
        _ => Vec::new(),
    }
}

/// First object entry of a tool-call's arguments as a `(key, short value)`
/// hint, mirroring the retired bridge's `summary_key`/`summary`.
fn arg_hint(arguments: &serde_json::Value) -> Option<(String, String)> {
    arguments.as_object()?.iter().next().map(|(key, value)| {
        let value = match value {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let short: String = value.chars().take(40).collect();
        (key.clone(), short)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi::session::SessionTreeEntry;

    fn message_entry(id: &str, text: &str) -> SessionTreeEntry {
        SessionTreeEntry::Message {
            id: id.to_string(),
            parent_id: None,
            timestamp: chrono::Utc::now(),
            message: AgentMessage::user(text),
        }
    }

    fn note_entry(text: &str) -> SessionTreeEntry {
        SessionTreeEntry::Custom {
            id: format!("note-{text}"),
            parent_id: None,
            timestamp: chrono::Utc::now(),
            custom_type: UI_NOTE_CUSTOM_TYPE.to_string(),
            data: Some(serde_json::json!({
                "kind": "error",
                "data": { "text": text },
            })),
        }
    }

    #[test]
    fn entries_to_display_interleaves_notes_and_skips_unknown_customs() {
        let entries = vec![
            message_entry("m1", "one"),
            note_entry("boom"),
            SessionTreeEntry::Custom {
                id: "other".into(),
                parent_id: None,
                timestamp: chrono::Utc::now(),
                custom_type: "some_extension".into(),
                data: None,
            },
            message_entry("m2", "two"),
        ];
        let (display, notes) = entries_to_display(&entries);
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].after_message, 1);
        let kinds: Vec<&str> = display
            .iter()
            .map(|e| match e {
                HistoryEntry::Message(_) => "m",
                HistoryEntry::Note(_) => "n",
            })
            .collect();
        assert_eq!(kinds, vec!["m", "n", "m"]);
    }

    #[test]
    fn entries_to_display_counts_positions_over_projected_compaction_list() {
        // `sync_history` feeds the projected (post-compaction) entry list:
        // pre-boundary notes never reach this function (build_context_entries
        // drops them — covered by the pi crate's projection tests). The
        // projection of `m1 [dropped] m2 [kept] c1(keeps m2) m3` is
        // [c1, m2, note-kept, m3]; positions count the summary + m2.
        let entries = vec![
            SessionTreeEntry::Compaction {
                id: "c1".into(),
                parent_id: None,
                timestamp: chrono::Utc::now(),
                summary: "recap".into(),
                first_kept_entry_id: Some("m2".into()),
                tokens_before: 0,
                retained_tail: None,
                usage: None,
                details: None,
                from_hook: None,
            },
            message_entry("m2", "two"),
            note_entry("kept"),
            message_entry("m3", "three"),
        ];
        let (display, notes) = entries_to_display(&entries);
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].note.data["text"].as_str(), Some("kept"));
        // summary + m2 precede the kept note.
        assert_eq!(notes[0].after_message, 2);
        let kinds: Vec<&str> = display
            .iter()
            .map(|e| match e {
                HistoryEntry::Message(_) => "m",
                HistoryEntry::Note(_) => "n",
            })
            .collect();
        assert_eq!(kinds, vec!["m", "m", "n", "m"]);
    }
    use serde_json::json;

    #[test]
    fn tool_titles_carry_browser_targets() {
        assert_eq!(
            tool_title("ChromeUseOpen", &json!({"url": "https://example.com"})),
            "ChromeUseOpen https://example.com"
        );
        assert_eq!(
            tool_title(
                "ChromeUseOpen",
                &json!({"cdp_endpoint": "ws://127.0.0.1:9222/x"})
            ),
            "ChromeUseOpen ws://127.0.0.1:9222/x"
        );
        assert_eq!(
            tool_title("ChromeUseClick", &json!({"tab_id": 2, "ref": "e7"})),
            "ChromeUseClick tab 2 [e7]"
        );
        assert_eq!(
            tool_title("WebExploreScreenshot", &json!({"tab_id": 1})),
            "WebExploreScreenshot tab 1"
        );
        // Unrecognized shapes fall back to the bare name.
        assert_eq!(tool_title("ChromeUseClose", &json!({})), "ChromeUseClose");
        assert_eq!(tool_title("Bash", &json!({})), "Bash");
    }
}
