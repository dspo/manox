//! Durable journal rows for a child session's live event stream.
//!
//! The harness persistence middleware journals message rows only, so a child
//! transcript folded by the AHP projection showed text and nothing else:
//! `Thinking`/`ToolUse` blocks inside the assistant message row are dropped
//! by the text-only projection. This module maps the child's `AgentEvent`s
//! onto the same durable row vocabulary user sessions write
//! (`agent_thinking_delta`, `agent_text_delta`, `tool_call`, `tool_result`,
//! `tool_output_chunk`), plus one `turn_start`/`turn_finish` pair per run.
//!
//! The listener that consumes these rows appends them inline with each
//! emission, so a delta lands before the message row that closes its block
//! and a `tool_call` lands before its `tool_result` — the order the journal
//! fold depends on.

use crate::types::{AgentEvent, AgentMessage, AssistantMessageEvent, ContentBlock, StopReason};
use serde_json::{Value, json};

/// The journal rows one child event contributes, in append order. Kinds are
/// the wire names [`crate::session::SessionTreeEntry::from_kind_payload`]
/// accepts; payload keys are the variants' camelCase serde names.
pub fn child_journal_rows(event: &AgentEvent) -> Vec<(&'static str, Value)> {
    match event {
        // One turn per run: the kernel's per-round `TurnStart`/`TurnEnd`
        // bracket tool loops inside a single turn, so they do not map. The
        // facade that drives user threads emits one turn pair per prompt;
        // `AgentStart`/`AgentEnd` are the child's equivalent boundary.
        AgentEvent::AgentStart => vec![(
            "turn_start",
            json!({ "owner": crate::session::TurnOwner::for_current_process() }),
        )],
        AgentEvent::AgentEnd { messages } => {
            let (cancelled, failed) = terminal_outcome(messages);
            vec![(
                "turn_finish",
                json!({
                    "cancelled": cancelled,
                    "failed": failed,
                    "strandedSteerIds": [],
                }),
            )]
        }
        AgentEvent::MessageUpdate {
            assistant_message_event,
            ..
        } => match assistant_message_event {
            AssistantMessageEvent::ThinkingDelta { delta, .. } => {
                vec![("agent_thinking_delta", json!({ "delta": delta }))]
            }
            AssistantMessageEvent::TextDelta { delta, .. } => {
                vec![("agent_text_delta", json!({ "delta": delta }))]
            }
            _ => Vec::new(),
        },
        AgentEvent::ToolExecutionStart {
            tool_call_id,
            tool_name,
            arguments,
        } => vec![(
            "tool_call",
            json!({
                "callId": tool_call_id,
                "name": tool_name,
                "title": tool_title(tool_name, arguments),
                "status": "running",
                "input": arguments,
            }),
        )],
        AgentEvent::ToolExecutionUpdate {
            tool_call_id,
            partial_result,
            ..
        } => match partial_result.get("output").and_then(Value::as_str) {
            Some(chunk) if !chunk.is_empty() => vec![(
                "tool_output_chunk",
                json!({ "callId": tool_call_id, "chunk": chunk }),
            )],
            _ => Vec::new(),
        },
        AgentEvent::ToolExecutionEnd {
            tool_call_id,
            tool_name,
            result,
            is_error,
        } => vec![
            (
                "tool_call",
                json!({
                    "callId": tool_call_id,
                    "name": tool_name,
                    "title": tool_name,
                    "status": if *is_error { "error" } else { "success" },
                    // The start row already published the arguments; a
                    // status row that carries them again would re-arm the
                    // projection's catch-up.
                    "input": Value::Null,
                }),
            ),
            (
                "tool_result",
                json!({
                    "callId": tool_call_id,
                    "output": tool_result_text(result),
                    "isError": is_error,
                }),
            ),
        ],
        _ => Vec::new(),
    }
}

/// The run's terminal outcome from its last assistant message.
fn terminal_outcome(messages: &[AgentMessage]) -> (bool, bool) {
    let stop = messages.iter().rev().find_map(|message| match message {
        AgentMessage::Assistant { stop_reason, .. } => Some(*stop_reason),
        _ => None,
    });
    match stop.flatten() {
        Some(StopReason::Aborted) => (true, false),
        Some(StopReason::Error) => (false, true),
        _ => (false, false),
    }
}

/// The text of a tool result, joined like the wire's `blocks_text`.
fn tool_result_text(result: &crate::tool::AgentToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Display-only call title, mirroring the host's card-title derivation (the
/// same target-first shapes) so a child's tool card reads like a user
/// session's. Unrecognized names fall back to their first string argument,
/// then to the bare name.
fn tool_title(name: &str, args: &Value) -> String {
    let arg = |key: &str| {
        args.get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
    };
    let title = match name {
        "Read" | "Write" | "Ls" => arg("path").map(|path| format!("{name} {path}")),
        "Edit" => match edit_patch_paths(args).as_slice() {
            [] => None,
            [only] => Some(format!("Edit {only}")),
            [first, rest @ ..] => Some(format!("Edit {first} +{}", rest.len())),
        },
        "Grep" => match (arg("pattern"), arg("path")) {
            (Some(pattern), Some(path)) => Some(format!("Grep {pattern} {path}")),
            (Some(pattern), None) => Some(format!("Grep {pattern}")),
            _ => None,
        },
        "Glob" => arg("pattern").map(|pattern| format!("Glob {pattern}")),
        "Bash" => arg("command").map(|command| format!("$ {command}")),
        // The body is the argument; the header names only the intent.
        "Code" => Some("Code".to_string()),
        "Py" => arg("script")
            .and_then(|script| script.lines().next())
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(|line| format!("Py {}", short_value(line, 40))),
        other => first_string_arg(args).map(|value| format!("{other} {}", short_value(&value, 40))),
    };
    title.unwrap_or_else(|| name.to_string())
}

/// File paths a hashline patch touches, parsed from its `[<path>#<tag>]`
/// section headers (display only; the tag is not validated).
fn edit_patch_paths(args: &Value) -> Vec<String> {
    let Some(patch) = args.get("patch").and_then(Value::as_str) else {
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

/// One pair of surrounding quotes stripped — hashline section headers quote
/// paths containing spaces.
fn unquote_path(path: &str) -> &str {
    let path = path.trim();
    if path.len() >= 2
        && ((path.starts_with('"') && path.ends_with('"'))
            || (path.starts_with('\'') && path.ends_with('\'')))
    {
        &path[1..path.len() - 1]
    } else {
        path
    }
}

/// Char-capped copy with an ellipsis when it was clipped.
fn short_value(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let head: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// The first string-typed argument value (unknown tools use it as a generic
/// title hint).
fn first_string_arg(args: &Value) -> Option<String> {
    args.as_object()?
        .values()
        .find_map(|value| value.as_str().filter(|s| !s.is_empty()).map(str::to_string))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::AssistantMessageEvent;

    fn kinds(rows: &[(&'static str, Value)]) -> Vec<&'static str> {
        rows.iter().map(|(kind, _)| *kind).collect()
    }

    fn assistant(content: Vec<ContentBlock>, stop_reason: Option<StopReason>) -> AgentMessage {
        AgentMessage::Assistant {
            content,
            model: "test-model".into(),
            provider: "test-provider".into(),
            api: "anthropic".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            stop_reason,
            raw_stop_reason: None,
            usage: Box::default(),
            error_message: None,
            timestamp: chrono::Utc::now(),
        }
    }

    #[test]
    fn deltas_map_to_their_row_kinds() {
        let thinking = child_journal_rows(&AgentEvent::MessageUpdate {
            message: Box::new(assistant(Vec::new(), None)),
            assistant_message_event: AssistantMessageEvent::ThinkingDelta {
                content_index: 0,
                delta: "pondering".into(),
            },
        });
        assert_eq!(kinds(&thinking), ["agent_thinking_delta"]);
        assert_eq!(thinking[0].1, json!({ "delta": "pondering" }));

        let text = child_journal_rows(&AgentEvent::MessageUpdate {
            message: Box::new(assistant(Vec::new(), None)),
            assistant_message_event: AssistantMessageEvent::TextDelta {
                content_index: 1,
                delta: "answer".into(),
            },
        });
        assert_eq!(kinds(&text), ["agent_text_delta"]);
        assert_eq!(text[0].1, json!({ "delta": "answer" }));
    }

    #[test]
    fn a_tool_call_lands_before_its_result_and_keeps_the_start_arguments() {
        let start = child_journal_rows(&AgentEvent::ToolExecutionStart {
            tool_call_id: "call-1".into(),
            tool_name: "Read".into(),
            arguments: json!({ "path": "src/lib.rs" }),
        });
        assert_eq!(kinds(&start), ["tool_call"]);
        assert_eq!(start[0].1["title"], "Read src/lib.rs");
        assert_eq!(start[0].1["status"], "running");
        assert_eq!(start[0].1["input"], json!({ "path": "src/lib.rs" }));

        let end = child_journal_rows(&AgentEvent::ToolExecutionEnd {
            tool_call_id: "call-1".into(),
            tool_name: "Read".into(),
            result: crate::tool::AgentToolResult::text("contents"),
            is_error: false,
        });
        assert_eq!(kinds(&end), ["tool_call", "tool_result"]);
        assert_eq!(end[0].1["status"], "success");
        assert_eq!(end[0].1["input"], Value::Null);
        assert_eq!(
            end[1].1,
            json!({
                "callId": "call-1",
                "output": "contents",
                "isError": false,
            })
        );
    }

    #[test]
    fn a_failed_tool_keeps_the_error_bit_on_both_rows() {
        let rows = child_journal_rows(&AgentEvent::ToolExecutionEnd {
            tool_call_id: "call-2".into(),
            tool_name: "Bash".into(),
            result: crate::tool::AgentToolResult {
                content: vec![ContentBlock::Text {
                    text: "boom".into(),
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
        assert_eq!(rows[0].1["status"], "error");
        assert_eq!(rows[1].1["isError"], true);
    }

    #[test]
    fn a_run_brackets_the_transcript_with_one_turn_pair() {
        let start = child_journal_rows(&AgentEvent::AgentStart);
        assert_eq!(kinds(&start), ["turn_start"]);
        assert!(start[0].1.get("owner").is_some());

        let abort = child_journal_rows(&AgentEvent::AgentEnd {
            messages: vec![assistant(Vec::new(), Some(StopReason::Aborted))],
        });
        assert_eq!(kinds(&abort), ["turn_finish"]);
        assert_eq!(abort[0].1["cancelled"], true);
        assert_eq!(abort[0].1["failed"], false);

        let ok = child_journal_rows(&AgentEvent::AgentEnd {
            messages: vec![assistant(
                vec![ContentBlock::Text {
                    text: "done".into(),
                    signature: None,
                }],
                Some(StopReason::Stop),
            )],
        });
        assert_eq!(ok[0].1["cancelled"], false);
        assert_eq!(ok[0].1["failed"], false);
    }

    #[test]
    fn partial_output_maps_to_a_chunk_row() {
        let rows = child_journal_rows(&AgentEvent::ToolExecutionUpdate {
            tool_call_id: "call-3".into(),
            tool_name: "Bash".into(),
            arguments: json!({}),
            partial_result: json!({ "output": "half" }),
        });
        assert_eq!(kinds(&rows), ["tool_output_chunk"]);
        assert_eq!(rows[0].1["chunk"], "half");

        let empty = child_journal_rows(&AgentEvent::ToolExecutionUpdate {
            tool_call_id: "call-3".into(),
            tool_name: "Bash".into(),
            arguments: json!({}),
            partial_result: json!({ "output": "" }),
        });
        assert!(empty.is_empty());
    }

    #[test]
    fn tool_titles_name_the_call_target() {
        assert_eq!(
            tool_title("Edit", &json!({"patch": "[a.rs#ABCD]\n[x]\n[b.rs#EFGH]"})),
            "Edit a.rs +1"
        );
        assert_eq!(
            tool_title("Edit", &json!({"patch": "[\"dir/a b.rs\"#ABCD]"})),
            "Edit dir/a b.rs"
        );
        assert_eq!(
            tool_title("Unknown", &json!({"target": "thing"})),
            "Unknown thing"
        );
        assert_eq!(tool_title("Bash", &json!({})), "Bash");
    }
}
