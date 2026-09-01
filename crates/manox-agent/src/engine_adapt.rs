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
/// `ToolCallAuthorization` never comes from this mapping — the permission
/// gate (`pi_approval`) emits it directly while parked on a user answer.
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
            Some(PiStopReason::Error) => {
                vec![ThreadEvent::Error(anyhow::anyhow!(
                    "{}",
                    message_error_text(message)
                ))]
            }
            Some(reason) => vec![ThreadEvent::Stop(manox_stop_reason_of(&reason))],
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
                    health: None,
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
                    health: None,
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
                                output: String::new(),
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
                    health: None,
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

/// Restore mapping: pi harness history onto the `manox_agent::Message` history
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

/// Map a pi stop reason onto the manox shape. `Error` never routes through
/// this helper: callers surface it as an `Error` event instead of a stop.
fn manox_stop_reason_of(reason: &PiStopReason) -> ManoxStopReason {
    match reason {
        PiStopReason::Stop => ManoxStopReason::EndTurn,
        PiStopReason::Length => ManoxStopReason::MaxTokens,
        PiStopReason::ToolUse => ManoxStopReason::ToolUse,
        PiStopReason::Aborted => ManoxStopReason::Cancelled,
        PiStopReason::Error => ManoxStopReason::EndTurn,
    }
}

/// Human-readable tool card title from the pi tool name + arguments. Each
/// arm surfaces the tool's decision-relevant target (path, url, command,
/// agent address, …); browser tools (WebExplore* / ChromeUse*) carry their
/// url, tab id, or element ref. Unrecognized tools fall back to their first
/// string argument, then to the bare name.
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
        "Edit" => match edit_patch_paths(args).as_slice() {
            [] => "Edit".to_string(),
            [only] => format!("Edit {only}"),
            [first, rest @ ..] => format!("Edit {first} +{}", rest.len()),
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
        "BashOutput" => match arg("shell_id") {
            Some(shell_id) => format!("BashOutput {shell_id}"),
            None => "BashOutput".to_string(),
        },
        "TaskStop" => match arg("task_id") {
            Some(task_id) => format!("TaskStop {task_id}"),
            None => "TaskStop".to_string(),
        },
        "Monitor" => {
            let ws_url = || {
                args.get("ws")
                    .and_then(|w| w.get("url"))
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
            };
            match arg("command")
                .or_else(ws_url)
                .or_else(|| arg("description"))
            {
                Some(target) => format!("Monitor {target}"),
                None => "Monitor".to_string(),
            }
        }
        "WebFetch" => match arg("url") {
            Some(url) => format!("WebFetch {url}"),
            None => "WebFetch".to_string(),
        },
        "Steer" => {
            let to = args.get("to");
            let addr = to
                .and_then(|t| t.get("agent_address"))
                .and_then(|v| v.as_str());
            let spawn = to.and_then(|t| t.get("spawn")).and_then(|v| v.as_str());
            match (spawn, addr) {
                (Some(spawn), Some(addr)) => format!("Steer {spawn} {addr}"),
                (Some(spawn), None) => format!("Steer {spawn}"),
                (None, Some(addr)) => match arg("reason") {
                    Some(reason) => format!("Steer {reason} {addr}"),
                    None => format!("Steer {addr}"),
                },
                (None, None) => "Steer".to_string(),
            }
        }
        "Agent" => match arg("subagent_type") {
            Some(kind) => format!("Agent {kind}"),
            None => "Agent".to_string(),
        },
        "Skill" => match arg("skill").or_else(|| arg("name")) {
            Some(skill) => format!("Skill {skill}"),
            None => "Skill".to_string(),
        },
        // Code carries a code body as its argument: the bare name keeps the
        // body out of the header.
        "Code" => "Code".to_string(),
        "ProposePlan" => match arg("title").or_else(|| arg("slug")) {
            Some(label) => format!("ProposePlan {label}"),
            None => "ProposePlan".to_string(),
        },
        "UpdatePlan" => match args.get("plan").and_then(|v| v.as_array()) {
            Some(steps) => format!("UpdatePlan ({} steps)", steps.len()),
            None => "UpdatePlan".to_string(),
        },
        // Task tools were removed in the tools-optimization cycle — they
        // were retired with the Steer-based team architecture.
        "CreateGoal" => match arg("objective") {
            Some(objective) => format!("CreateGoal {}", short_value(&objective, 40)),
            None => "CreateGoal".to_string(),
        },
        "UpdateGoal" => match arg("status") {
            Some(status) => format!("UpdateGoal {status}"),
            None => "UpdateGoal".to_string(),
        },
        "GoToDefinition" | "FindReferences" | "Hover" => {
            let line = args.get("line").and_then(|v| v.as_u64());
            match (arg("path"), line, arg("symbol")) {
                (Some(path), Some(line), Some(symbol)) => {
                    format!("{name} {path}:{line} {symbol}")
                }
                (Some(path), Some(line), None) => format!("{name} {path}:{line}"),
                (Some(path), None, _) => format!("{name} {path}"),
                _ => name.to_string(),
            }
        }
        "DocumentSymbols" | "Diagnostics" => match arg("path") {
            Some(path) => format!("{name} {path}"),
            None => name.to_string(),
        },
        "WorkspaceSymbols" => match arg("query") {
            Some(query) => format!("WorkspaceSymbols {query}"),
            None => "WorkspaceSymbols".to_string(),
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
        other => match first_string_arg(args) {
            Some(value) => format!("{other} {}", short_value(&value, 40)),
            None => other.to_string(),
        },
    }
}

/// File paths a hashline patch touches, parsed from its `[<path>#<tag>]`
/// section headers (display-only; the tag is not validated).
fn edit_patch_paths(args: &serde_json::Value) -> Vec<String> {
    let Some(patch) = args.get("patch").and_then(|v| v.as_str()) else {
        return Vec::new();
    };
    patch
        .lines()
        .filter_map(|line| {
            let rest = line.trim().strip_prefix('[')?.strip_suffix(']')?;
            let (path, tag) = rest.rsplit_once('#')?;
            if path.is_empty() || tag.is_empty() {
                return None;
            }
            Some(unquote_path(path).to_string())
        })
        .collect()
}

/// One pair of surrounding single/double quotes stripped — hashline section
/// headers quote paths containing spaces.
fn unquote_path(s: &str) -> &str {
    let s = s.trim();
    if s.len() >= 2
        && ((s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')))
    {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// Char-capped copy of a string with an ellipsis when it was clipped.
fn short_value(s: &str, max_chars: usize) -> String {
    let mut chars = s.chars();
    let head: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// The first string-typed argument value of a tool call (unknown/MCP tools
/// use it as a generic title hint). Iteration follows the model's argument
/// order (workspace-wide serde_json `preserve_order`), so one call always
/// resolves the same value.
fn first_string_arg(args: &serde_json::Value) -> Option<String> {
    args.as_object()?
        .values()
        .find_map(|v| v.as_str().filter(|s| !s.is_empty()).map(str::to_string))
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
        health: None,
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
            result,
            is_error,
            ..
        } => vec![ThreadEvent::SubagentChild {
            id: id.to_string(),
            child: crate::thread::SubagentChildEvent::ToolEnd {
                id: tool_call_id.clone(),
                name: tool_name.clone(),
                is_error: *is_error,
                output: tool_result_text(result),
            },
        }],
        // The child's assistant message finished streaming. The stop reason
        // mirrors the main thread's `MessageEnd` mapping; the message usage
        // rides along so the panel can stamp the just-finished reply.
        AgentEvent::MessageEnd { message } => match message_stop_reason(message) {
            Some(PiStopReason::Error) => vec![ThreadEvent::SubagentChild {
                id: id.to_string(),
                child: crate::thread::SubagentChildEvent::Error(message_error_text(message)),
            }],
            Some(reason) => {
                let usage = match &**message {
                    AgentMessage::Assistant { usage, .. } => Some(super::to_token_usage(usage)),
                    _ => None,
                };
                vec![ThreadEvent::SubagentChild {
                    id: id.to_string(),
                    child: crate::thread::SubagentChildEvent::Stop {
                        reason: manox_stop_reason_of(&reason),
                        usage,
                    },
                }]
            }
            None => Vec::new(),
        },
        // Run boundary the panel needs even though the main thread's facade
        // owns it: the last `MessageEnd` already sealed the tail, so this is
        // an idempotent backstop for runs that ended without one (provider
        // error streams that only emitted `Error`, aborts).
        AgentEvent::AgentEnd { .. } => vec![ThreadEvent::SubagentChild {
            id: id.to_string(),
            child: crate::thread::SubagentChildEvent::Stop {
                reason: ManoxStopReason::EndTurn,
                usage: None,
            },
        }],
        _ => Vec::new(),
    }
}

/// First object entry of a tool-call's arguments as a `(key, short value)`
/// hint, mirroring the retired bridge's `summary_key`/`summary`.
pub(crate) fn arg_hint(arguments: &serde_json::Value) -> Option<(String, String)> {
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

    #[test]
    fn tool_titles_surface_host_tool_targets() {
        // Steer: a spawn carries the def name + address; Inject/Abort carry
        // the reason + address.
        assert_eq!(
            tool_title(
                "Steer",
                &json!({
                    "to": {"agent_address": "sailor-1", "spawn": "Sailor"},
                    "reason": "Dispatch",
                    "prompt": "fix the bug"
                })
            ),
            "Steer Sailor sailor-1"
        );
        assert_eq!(
            tool_title(
                "Steer",
                &json!({
                    "to": {"agent_address": "sailor-1"},
                    "reason": "Inject",
                    "prompt": "a note"
                })
            ),
            "Steer Inject sailor-1"
        );
        assert_eq!(tool_title("Steer", &json!({})), "Steer");

        // Edit: paths come from the patch section headers.
        assert_eq!(
            tool_title(
                "Edit",
                &json!({"patch": "[/proj/src/main.rs#A1B2C3]\nSWAP 1.=2:\n+x"})
            ),
            "Edit /proj/src/main.rs"
        );
        assert_eq!(
            tool_title(
                "Edit",
                &json!({"patch": "[/a.rs#A1B2C3]\nDEL 1.=1\n[/b.rs#D4E5F6]\nDEL 1.=1"})
            ),
            "Edit /a.rs +1"
        );
        assert_eq!(tool_title("Edit", &json!({})), "Edit");
        // Quoted section headers (paths with spaces) surface unquoted.
        assert_eq!(
            tool_title(
                "Edit",
                &json!({"patch": "[\"/my docs/a b.rs\"#A1B2C3]\nDEL 1.=1"})
            ),
            "Edit /my docs/a b.rs"
        );

        assert_eq!(tool_title("Skill", &json!({"skill": "gpui"})), "Skill gpui");
        // Code keeps the bare name — its argument is a code body.
        assert_eq!(tool_title("Code", &json!({"code": "let x = 1;"})), "Code");

        assert_eq!(
            tool_title("ProposePlan", &json!({"slug": "s", "title": "My Plan"})),
            "ProposePlan My Plan"
        );
        assert_eq!(
            tool_title("ProposePlan", &json!({"slug": "my-slug"})),
            "ProposePlan my-slug"
        );
        assert_eq!(
            tool_title(
                "UpdatePlan",
                &json!({"plan": [
                    {"status": "pending", "step": "a"},
                    {"status": "pending", "step": "b"}
                ]})
            ),
            "UpdatePlan (2 steps)"
        );
        // Task tools were removed in the tools-optimization cycle.
        assert_eq!(tool_title("GetGoal", &json!({})), "GetGoal");
        assert_eq!(
            tool_title("UpdateGoal", &json!({"status": "complete"})),
            "UpdateGoal complete"
        );
        assert_eq!(
            tool_title("WebFetch", &json!({"url": "https://example.com/doc"})),
            "WebFetch https://example.com/doc"
        );
        assert_eq!(
            tool_title(
                "Monitor",
                &json!({"description": "watch", "command": "cargo build"})
            ),
            "Monitor cargo build"
        );
        assert_eq!(
            tool_title(
                "Monitor",
                &json!({"description": "watch", "ws": {"url": "wss://x/ws"}})
            ),
            "Monitor wss://x/ws"
        );
        assert_eq!(
            tool_title("BashOutput", &json!({"shell_id": "sh_3"})),
            "BashOutput sh_3"
        );
        assert_eq!(
            tool_title("TaskStop", &json!({"task_id": "mon_2"})),
            "TaskStop mon_2"
        );
        assert_eq!(
            tool_title(
                "GoToDefinition",
                &json!({"path": "src/main.rs", "line": 12, "symbol": "run"})
            ),
            "GoToDefinition src/main.rs:12 run"
        );
        assert_eq!(
            tool_title("DocumentSymbols", &json!({"path": "src/main.rs"})),
            "DocumentSymbols src/main.rs"
        );
        assert_eq!(
            tool_title("WorkspaceSymbols", &json!({"query": "tool_title"})),
            "WorkspaceSymbols tool_title"
        );
    }

    #[test]
    fn create_goal_title_caps_the_objective() {
        let long = "x".repeat(60);
        let title = tool_title("CreateGoal", &json!({"objective": long}));
        let rest = title
            .strip_prefix("CreateGoal ")
            .expect("CreateGoal prefix");
        assert_eq!(rest.chars().count(), 41, "40 kept chars + ellipsis");
        assert!(rest.ends_with('…'));
    }

    #[test]
    fn unknown_tools_fall_back_to_first_string_arg() {
        assert_eq!(
            tool_title(
                "mcp_some_server_query",
                &json!({"limit": 10, "query": "open issues"})
            ),
            "mcp_some_server_query open issues"
        );
        assert_eq!(
            tool_title("mcp_no_strings", &json!({"limit": 10})),
            "mcp_no_strings"
        );
        let long = "y".repeat(50);
        let title = tool_title("mcp_long", &json!({"text": long}));
        assert_eq!(title, format!("mcp_long {}…", "y".repeat(40)));
    }

    fn assistant_msg(
        stop_reason: Option<pi::types::StopReason>,
        input_tokens: u64,
    ) -> pi::types::AgentMessage {
        pi::types::AgentMessage::Assistant {
            content: vec![],
            model: "m".into(),
            provider: "p".into(),
            api: "anthropic".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            stop_reason,
            raw_stop_reason: None,
            usage: Box::new(pi::types::Usage {
                input_tokens,
                output_tokens: 7,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                total_tokens: 0,
                reasoning_tokens: None,
                cost: None,
                cache_write_1h: None,
            }),
            error_message: None,
            timestamp: chrono::Utc::now(),
        }
    }

    fn child_events_of_msg_end(stop_reason: pi::types::StopReason) -> Vec<ThreadEvent> {
        child_events_of(
            "sub-1",
            &pi::types::AgentEvent::MessageEnd {
                message: Box::new(assistant_msg(Some(stop_reason), 11)),
            },
        )
    }

    /// The child's message boundary maps onto `Stop` with the manox reason
    /// and the message's token usage, mirroring the main thread's mapping.
    #[test]
    fn child_message_end_maps_to_stop_with_usage() {
        let events = child_events_of_msg_end(pi::types::StopReason::ToolUse);
        assert_eq!(events.len(), 1);
        match &events[0] {
            crate::thread::ThreadEvent::SubagentChild {
                id,
                child: crate::thread::SubagentChildEvent::Stop { reason, usage },
            } => {
                assert_eq!(id, "sub-1");
                assert_eq!(*reason, ManoxStopReason::ToolUse);
                let usage = usage.as_ref().unwrap();
                assert_eq!(usage.input_tokens, 11);
                assert_eq!(usage.output_tokens, 7);
            }
            other => panic!("expected Stop, got {other:?}"),
        }

        let events = child_events_of_msg_end(pi::types::StopReason::Stop);
        assert!(matches!(
            &events[0],
            crate::thread::ThreadEvent::SubagentChild {
                child: crate::thread::SubagentChildEvent::Stop {
                    reason: ManoxStopReason::EndTurn,
                    ..
                },
                ..
            }
        ));

        let events = child_events_of_msg_end(pi::types::StopReason::Length);
        assert!(matches!(
            &events[0],
            crate::thread::ThreadEvent::SubagentChild {
                child: crate::thread::SubagentChildEvent::Stop {
                    reason: ManoxStopReason::MaxTokens,
                    ..
                },
                ..
            }
        ));

        let events = child_events_of_msg_end(pi::types::StopReason::Aborted);
        assert!(matches!(
            &events[0],
            crate::thread::ThreadEvent::SubagentChild {
                child: crate::thread::SubagentChildEvent::Stop {
                    reason: ManoxStopReason::Cancelled,
                    ..
                },
                ..
            }
        ));
    }

    /// A terminal provider error message maps onto `Error` with its text.
    #[test]
    fn child_error_message_maps_to_error_event() {
        let message = pi::types::AgentMessage::Assistant {
            content: vec![],
            model: "m".into(),
            provider: "p".into(),
            api: "anthropic".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            stop_reason: Some(pi::types::StopReason::Error),
            raw_stop_reason: None,
            usage: Box::new(pi::types::Usage::default()),
            error_message: Some("context window exhausted".into()),
            timestamp: chrono::Utc::now(),
        };
        let events = child_events_of(
            "sub-1",
            &pi::types::AgentEvent::MessageEnd {
                message: Box::new(message),
            },
        );
        match &events[0] {
            crate::thread::ThreadEvent::SubagentChild {
                child: crate::thread::SubagentChildEvent::Error(text),
                ..
            } => assert_eq!(text, "context window exhausted"),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    /// A finished tool call carries the result text for the tool card, and
    /// the run boundary seals the transcript (idempotent with the last
    /// `MessageEnd` stop).
    #[test]
    fn child_tool_end_carries_output_and_agent_end_seals() {
        let events = child_events_of(
            "sub-1",
            &pi::types::AgentEvent::ToolExecutionEnd {
                tool_call_id: "c1".into(),
                tool_name: "Read".into(),
                result: pi::tool::AgentToolResult::text("found it"),
                is_error: false,
            },
        );
        assert_eq!(events.len(), 1);
        match &events[0] {
            crate::thread::ThreadEvent::SubagentChild {
                child:
                    crate::thread::SubagentChildEvent::ToolEnd {
                        id,
                        output,
                        is_error,
                        ..
                    },
                ..
            } => {
                assert_eq!(id, "c1");
                assert_eq!(output, "found it");
                assert!(!is_error);
            }
            other => panic!("expected ToolEnd, got {other:?}"),
        }

        let events = child_events_of(
            "sub-1",
            &pi::types::AgentEvent::AgentEnd { messages: vec![] },
        );
        assert!(matches!(
            &events[0],
            crate::thread::ThreadEvent::SubagentChild {
                child: crate::thread::SubagentChildEvent::Stop {
                    reason: ManoxStopReason::EndTurn,
                    usage: None,
                },
                ..
            }
        ));
    }
}
