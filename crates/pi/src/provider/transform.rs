// Request-time transcript preparation shared by every provider shape.
//
// Two passes run in order. `convert_to_llm` projects the harness-only roles
// onto the three the wire protocols accept, so a shape translator only ever
// matches user/assistant/toolResult. `repair_tool_flow` then pairs every tool
// call whose result never arrived — a turn whose persistence failed partway,
// or a run interrupted between the call and its result — with a synthetic
// error result, since providers reject a bare tool call and would otherwise
// make the session impossible to continue.

use std::collections::HashSet;

use crate::types::{AgentMessage, ContentBlock, StopReason};

/// The body of the synthetic result standing in for a tool call whose
/// result never made it into the transcript.
const NO_RESULT_TEXT: &str = "No result provided";

/// Project harness roles onto wire roles, then repair the tool flow.
///
/// The order matters: a projected `Custom` message is a user message, and a
/// user message closes an open tool turn, so converting first is what makes a
/// custom message between a tool call and its result end that turn.
pub fn prepare_for_wire(messages: &[AgentMessage]) -> Vec<AgentMessage> {
    repair_tool_flow(&convert_to_llm(messages))
}

/// Project harness-only message roles onto the wire roles.
///
/// A `Custom` message carries content the model is meant to read — a
/// harness-injected note, a resource digest — and reaches the provider as a
/// user message, so the roles a shape translator must handle reduce to
/// user/assistant/toolResult. Its blocks carry over untouched, images
/// included. A `BashExecution` becomes the rendered transcript of the command,
/// or drops entirely when it was recorded with `exclude_from_context`.
///
/// Summary carriers are already user messages by the time they are here: the
/// session projects a compaction or branch-summary entry into a tagged user
/// message when it rebuilds the transcript, so this pass leaves them alone
/// rather than wrapping them twice.
pub fn convert_to_llm(messages: &[AgentMessage]) -> Vec<AgentMessage> {
    messages
        .iter()
        .filter_map(|msg| match msg {
            AgentMessage::Custom {
                content, timestamp, ..
            } => Some(AgentMessage::User {
                content: content.clone(),
                timestamp: *timestamp,
            }),
            AgentMessage::BashExecution {
                exclude_from_context: Some(true),
                ..
            } => None,
            AgentMessage::BashExecution { timestamp, .. } => Some(AgentMessage::User {
                content: vec![ContentBlock::Text {
                    text: bash_execution_to_text(msg),
                    signature: None,
                }],
                timestamp: *timestamp,
            }),
            other => Some(other.clone()),
        })
        .collect()
}

/// Render a shell execution as the text the model reads.
///
/// The command leads, its output follows fenced, and the trailing note carries
/// whatever the model needs to interpret the result: cancellation, a non-zero
/// status, or where the untruncated output went.
pub fn bash_execution_to_text(message: &AgentMessage) -> String {
    let AgentMessage::BashExecution {
        command,
        output,
        exit_code,
        cancelled,
        truncated,
        full_output_path,
        ..
    } = message
    else {
        return String::new();
    };

    let mut text = format!("Ran `{command}`\n");
    if output.is_empty() {
        text.push_str("(no output)");
    } else {
        text.push_str(&format!("```\n{output}\n```"));
    }
    if *cancelled {
        text.push_str("\n\n(command cancelled)");
    } else if let Some(code) = exit_code.filter(|c| *c != 0) {
        text.push_str(&format!("\n\nCommand exited with code {code}"));
    }
    if *truncated && let Some(path) = full_output_path {
        text.push_str(&format!("\n\n[Output truncated. Full output: {path}]"));
    }
    text
}

/// Pair every unresolved tool call with a synthetic error result.
///
/// A call counts as resolved when a later tool result names its id;
/// otherwise a result reading "No result provided" is inserted where the
/// call's turn ends — before the next assistant or user message, or at the
/// end of the transcript. Clean transcripts pass through unchanged.
///
/// On the wire path `convert_to_llm` runs first, so a `Custom` arrives here
/// already a user message and closes an open tool turn. Compaction reasons
/// about unconverted transcripts, where a `Custom` sits mid tool chain
/// without closing it — the cut analysis in `find_safe_cut` depends on that
/// shape being reachable.
pub fn repair_tool_flow(messages: &[AgentMessage]) -> Vec<AgentMessage> {
    let mut result = Vec::with_capacity(messages.len());
    // Tool calls of the latest assistant turn, awaiting their results.
    let mut pending: Vec<(String, String)> = Vec::new();
    let mut resolved: HashSet<String> = HashSet::new();

    for msg in messages {
        match msg {
            AgentMessage::Assistant {
                content,
                stop_reason,
                ..
            } => {
                // Close out the previous turn's tool calls first — a terminal
                // assistant still marks the end of the prior turn.
                insert_synthetic_results(&mut result, &mut pending, &mut resolved);
                // TS Pi's transformMessages drops assistants that ended in
                // `Error`/`Aborted`: their reasoning and tool calls may be
                // incomplete and must not be replayed, so neither the message
                // nor its calls enter the wire transcript.
                if matches!(
                    stop_reason,
                    Some(StopReason::Error) | Some(StopReason::Aborted)
                ) {
                    continue;
                }
                let calls: Vec<(String, String)> = content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::ToolUse { id, name, .. } => Some((id.clone(), name.clone())),
                        _ => None,
                    })
                    .collect();
                if !calls.is_empty() {
                    pending = calls;
                }
                result.push(msg.clone());
            }
            AgentMessage::ToolResult { tool_call_id, .. } => {
                resolved.insert(tool_call_id.clone());
                result.push(msg.clone());
            }
            AgentMessage::User { .. } => {
                insert_synthetic_results(&mut result, &mut pending, &mut resolved);
                result.push(msg.clone());
            }
            AgentMessage::BashExecution { .. } | AgentMessage::Custom { .. } => {
                result.push(msg.clone())
            }
        }
    }
    insert_synthetic_results(&mut result, &mut pending, &mut resolved);
    result
}

/// Close out the pending tool calls with synthetic error results, one per
/// call that no recorded result answered.
fn insert_synthetic_results(
    result: &mut Vec<AgentMessage>,
    pending: &mut Vec<(String, String)>,
    resolved: &mut HashSet<String>,
) {
    for (id, name) in pending.drain(..) {
        if resolved.contains(&id) {
            continue;
        }
        result.push(AgentMessage::ToolResult {
            tool_call_id: id,
            tool_name: name,
            content: vec![ContentBlock::Text {
                text: NO_RESULT_TEXT.to_string(),
                signature: None,
            }],
            is_error: true,
            details: None,
            usage: None,
            added_tool_names: None,
            timestamp: chrono::Utc::now(),
        });
    }
    resolved.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{StopReason, Usage};

    fn assistant(content: Vec<ContentBlock>) -> AgentMessage {
        AgentMessage::Assistant {
            content,
            model: "test".into(),
            provider: "test".into(),
            api: "test".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            raw_stop_reason: None,
            stop_reason: Some(StopReason::Stop),
            usage: Box::new(Usage::default()),
            error_message: None,
            timestamp: chrono::Utc::now(),
        }
    }

    /// An assistant that ended mid-turn in `Error`/`Aborted` — its content may
    /// be partial and must not survive the wire transform.
    fn terminal_assistant(content: Vec<ContentBlock>, stop_reason: StopReason) -> AgentMessage {
        AgentMessage::Assistant {
            content,
            model: "test".into(),
            provider: "test".into(),
            api: "test".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            raw_stop_reason: None,
            stop_reason: Some(stop_reason),
            usage: Box::new(Usage::default()),
            error_message: Some("interrupted".into()),
            timestamp: chrono::Utc::now(),
        }
    }

    fn tool_use(id: &str, name: &str) -> ContentBlock {
        ContentBlock::ToolUse {
            id: id.into(),
            name: name.into(),
            input: serde_json::json!({}),
            thought_signature: None,
        }
    }

    fn tool_result(id: &str) -> AgentMessage {
        AgentMessage::ToolResult {
            tool_call_id: id.into(),
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
        }
    }

    fn synthetic_at(messages: &[AgentMessage], index: usize) -> &AgentMessage {
        match &messages[index] {
            m @ AgentMessage::ToolResult { .. } => m,
            other => panic!("expected a synthetic tool result at {index}, got {other:?}"),
        }
    }

    #[test]
    fn orphan_at_end_gains_synthetic_error_result() {
        let messages = vec![
            AgentMessage::user("q"),
            assistant(vec![tool_use("c1", "read")]),
        ];
        let repaired = repair_tool_flow(&messages);
        assert_eq!(repaired.len(), 3);
        match synthetic_at(&repaired, 2) {
            AgentMessage::ToolResult {
                tool_call_id,
                tool_name,
                content,
                is_error,
                ..
            } => {
                assert_eq!(tool_call_id, "c1");
                assert_eq!(tool_name, "read");
                assert!(is_error);
                assert!(
                    matches!(&content[0], ContentBlock::Text { text, .. } if text == NO_RESULT_TEXT)
                );
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn orphan_interrupted_by_user_is_paired_before_it() {
        let messages = vec![
            assistant(vec![tool_use("c1", "read")]),
            AgentMessage::user("next"),
        ];
        let repaired = repair_tool_flow(&messages);
        assert_eq!(repaired.len(), 3);
        synthetic_at(&repaired, 1);
        assert!(matches!(&repaired[2], AgentMessage::User { .. }));
    }

    #[test]
    fn orphan_interrupted_by_assistant_is_paired_between() {
        let messages = vec![
            assistant(vec![tool_use("c1", "read")]),
            assistant(vec![ContentBlock::Text {
                text: "answer".into(),
                signature: None,
            }]),
        ];
        let repaired = repair_tool_flow(&messages);
        assert_eq!(repaired.len(), 3);
        synthetic_at(&repaired, 1);
        assert!(matches!(&repaired[2], AgentMessage::Assistant { .. }));
    }

    #[test]
    fn only_unresolved_calls_gain_synthetic_results() {
        let messages = vec![
            assistant(vec![tool_use("c1", "read"), tool_use("c2", "write")]),
            tool_result("c1"),
            AgentMessage::user("next"),
        ];
        let repaired = repair_tool_flow(&messages);
        // One real result, one synthetic for c2, then the user message.
        assert_eq!(repaired.len(), 4);
        match synthetic_at(&repaired, 2) {
            AgentMessage::ToolResult { tool_call_id, .. } => assert_eq!(tool_call_id, "c2"),
            _ => unreachable!(),
        }
    }

    #[test]
    fn resolved_turns_pass_through_unchanged() {
        let messages = vec![
            AgentMessage::user("q"),
            assistant(vec![tool_use("c1", "read")]),
            tool_result("c1"),
            assistant(vec![ContentBlock::Text {
                text: "done".into(),
                signature: None,
            }]),
        ];
        let repaired = repair_tool_flow(&messages);
        assert_eq!(
            serde_json::to_value(&repaired).unwrap(),
            serde_json::to_value(&messages).unwrap()
        );
    }

    #[test]
    fn unconverted_custom_messages_do_not_close_a_tool_turn() {
        let messages = vec![
            assistant(vec![tool_use("c1", "read")]),
            AgentMessage::Custom {
                custom_type: "note".into(),
                content: vec![],
                display: false,
                details: None,
                timestamp: chrono::Utc::now(),
            },
            AgentMessage::user("next"),
        ];
        let repaired = repair_tool_flow(&messages);
        // The synthetic result lands before the user message, after the
        // custom one — the custom message itself does not end the tool turn.
        // Compaction's cut analysis depends on this shape being reachable.
        assert_eq!(repaired.len(), 4);
        synthetic_at(&repaired, 2);
    }

    #[test]
    fn converted_custom_message_closes_the_tool_turn() {
        // On the wire path the custom message is a user message by the time
        // repair runs, so it ends the turn and the synthetic result precedes
        // it rather than following it.
        let messages = vec![
            assistant(vec![tool_use("c1", "read")]),
            AgentMessage::Custom {
                custom_type: "note".into(),
                content: vec![],
                display: false,
                details: None,
                timestamp: chrono::Utc::now(),
            },
            AgentMessage::user("next"),
        ];
        let prepared = prepare_for_wire(&messages);
        assert_eq!(prepared.len(), 4);
        synthetic_at(&prepared, 1);
    }

    #[test]
    fn custom_message_becomes_a_user_message() {
        let messages = vec![AgentMessage::Custom {
            custom_type: "note".into(),
            content: vec![ContentBlock::Text {
                text: "remember this".into(),
                signature: None,
            }],
            display: false,
            details: None,
            timestamp: chrono::Utc::now(),
        }];
        let converted = convert_to_llm(&messages);
        match &converted[0] {
            AgentMessage::User { content, .. } => assert!(matches!(
                &content[0],
                ContentBlock::Text { text, .. } if text == "remember this"
            )),
            other => panic!("custom message must project onto a user message: {other:?}"),
        }
    }

    #[test]
    fn custom_message_images_survive_conversion() {
        let messages = vec![AgentMessage::Custom {
            custom_type: "screenshot".into(),
            content: vec![
                ContentBlock::Text {
                    text: "look".into(),
                    signature: None,
                },
                ContentBlock::Image {
                    data: "aGk=".into(),
                    mime_type: "image/png".into(),
                },
            ],
            display: false,
            details: None,
            timestamp: chrono::Utc::now(),
        }];
        let converted = convert_to_llm(&messages);
        match &converted[0] {
            AgentMessage::User { content, .. } => {
                assert_eq!(content.len(), 2);
                assert!(matches!(
                    &content[1],
                    ContentBlock::Image { data, mime_type }
                        if data == "aGk=" && mime_type == "image/png"
                ));
            }
            other => panic!("expected a user message: {other:?}"),
        }
    }

    #[test]
    fn already_tagged_summary_carriers_are_not_wrapped_twice() {
        // The session projects compaction and branch summaries into tagged
        // user messages when it rebuilds the transcript; conversion leaves
        // them byte-identical rather than re-wrapping them.
        let now = chrono::Utc::now();
        let messages = vec![
            crate::session::compaction_summary_message("history", now),
            crate::session::branch_summary_message("branch", now),
        ];
        let converted = convert_to_llm(&messages);
        assert_eq!(
            serde_json::to_value(&converted).unwrap(),
            serde_json::to_value(&messages).unwrap()
        );
    }

    fn bash(output: &str, exit_code: Option<i32>, cancelled: bool) -> AgentMessage {
        AgentMessage::BashExecution {
            command: "ls".into(),
            output: output.into(),
            exit_code,
            cancelled,
            truncated: false,
            full_output_path: None,
            exclude_from_context: None,
            timestamp: chrono::Utc::now(),
        }
    }

    #[test]
    fn bash_execution_renders_every_trailing_note() {
        assert_eq!(
            bash_execution_to_text(&bash("hi", Some(0), false)),
            "Ran `ls`\n```\nhi\n```"
        );
        assert_eq!(
            bash_execution_to_text(&bash("", Some(0), false)),
            "Ran `ls`\n(no output)"
        );
        assert_eq!(
            bash_execution_to_text(&bash("hi", None, true)),
            "Ran `ls`\n```\nhi\n```\n\n(command cancelled)"
        );
        assert_eq!(
            bash_execution_to_text(&bash("hi", Some(1), false)),
            "Ran `ls`\n```\nhi\n```\n\nCommand exited with code 1"
        );
        // Cancellation wins over an exit code, matching the recorded shape
        // where a killed process reports no status.
        assert_eq!(
            bash_execution_to_text(&bash("hi", Some(1), true)),
            "Ran `ls`\n```\nhi\n```\n\n(command cancelled)"
        );
    }

    #[test]
    fn truncated_bash_execution_points_at_the_full_output() {
        let msg = AgentMessage::BashExecution {
            command: "cargo test".into(),
            output: "tail".into(),
            exit_code: Some(0),
            cancelled: false,
            truncated: true,
            full_output_path: Some("/tmp/pi-bash-1.log".into()),
            exclude_from_context: None,
            timestamp: chrono::Utc::now(),
        };
        assert_eq!(
            bash_execution_to_text(&msg),
            "Ran `cargo test`\n```\ntail\n```\n\n[Output truncated. Full output: /tmp/pi-bash-1.log]"
        );
    }

    #[test]
    fn bash_execution_projects_to_user_text() {
        let converted = convert_to_llm(&[bash("hi", Some(0), false)]);
        match &converted[0] {
            AgentMessage::User { content, .. } => assert!(matches!(
                &content[0],
                ContentBlock::Text { text, .. } if text == "Ran `ls`\n```\nhi\n```"
            )),
            other => panic!("expected a user message: {other:?}"),
        }
    }

    #[test]
    fn excluded_bash_execution_never_reaches_the_provider() {
        let excluded = AgentMessage::BashExecution {
            command: "cat secret".into(),
            output: "token".into(),
            exit_code: Some(0),
            cancelled: false,
            truncated: false,
            full_output_path: None,
            exclude_from_context: Some(true),
            timestamp: chrono::Utc::now(),
        };
        let prepared = prepare_for_wire(&[AgentMessage::user("q"), excluded]);
        assert_eq!(prepared.len(), 1);
        // Not merely stripped of content — absent, so nothing hints at it.
        let wire = serde_json::to_string(&prepared).unwrap();
        assert!(!wire.contains("token"), "{wire}");
    }

    #[test]
    fn converted_bash_execution_closes_the_tool_turn() {
        let messages = vec![
            assistant(vec![tool_use("c1", "read")]),
            bash("hi", Some(0), false),
        ];
        let prepared = prepare_for_wire(&messages);
        // The synthetic result precedes the projected user message; a bare
        // tool call would otherwise be the last thing before it.
        assert_eq!(prepared.len(), 3);
        synthetic_at(&prepared, 1);
        assert!(matches!(&prepared[2], AgentMessage::User { .. }));
    }

    #[test]
    fn clean_transcripts_pass_through_wire_preparation_unchanged() {
        // The prefix-caching invariant: a transcript of only wire roles is
        // byte-identical before and after preparation, so cross-turn request
        // prefixes do not shift.
        let messages = vec![
            AgentMessage::user("q"),
            assistant(vec![tool_use("c1", "read")]),
            tool_result("c1"),
            assistant(vec![ContentBlock::Text {
                text: "done".into(),
                signature: None,
            }]),
        ];
        let prepared = prepare_for_wire(&messages);
        assert_eq!(
            serde_json::to_value(&prepared).unwrap(),
            serde_json::to_value(&messages).unwrap()
        );
    }

    #[test]
    fn error_assistant_is_dropped_with_its_tool_calls() {
        // An aborted turn with a tool call: both the assistant and its call
        // vanish — the call is partial and must not be paired or replayed.
        let messages = vec![
            AgentMessage::user("q"),
            terminal_assistant(vec![tool_use("c1", "read")], StopReason::Aborted),
            AgentMessage::user("retry"),
        ];
        let repaired = repair_tool_flow(&messages);
        assert_eq!(repaired.len(), 2);
        assert!(matches!(&repaired[0], AgentMessage::User { .. }));
        assert!(matches!(&repaired[1], AgentMessage::User { .. }));
    }

    #[test]
    fn error_assistant_still_closes_the_prior_turns_pending() {
        // A prior normal turn's unresolved call is paired before the terminal
        // assistant is dropped — the dropped turn still ends the prior one.
        let messages = vec![
            assistant(vec![tool_use("c1", "read")]),
            terminal_assistant(
                vec![ContentBlock::Text {
                    text: "boom".into(),
                    signature: None,
                }],
                StopReason::Error,
            ),
            AgentMessage::user("retry"),
        ];
        let repaired = repair_tool_flow(&messages);
        // user(prompt?) no — assistant1, synthetic for c1, user retry. The
        // terminal assistant is gone; its own (none) calls not paired.
        assert_eq!(repaired.len(), 3);
        match synthetic_at(&repaired, 1) {
            AgentMessage::ToolResult { tool_call_id, .. } => assert_eq!(tool_call_id, "c1"),
            _ => unreachable!(),
        }
        assert!(matches!(&repaired[2], AgentMessage::User { .. }));
    }
}
