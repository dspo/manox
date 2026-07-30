// Request-time transcript repair shared by every provider shape.
//
// A transcript can contain tool calls whose result never arrived — a turn
// whose persistence failed partway, or a run interrupted between the
// assistant's tool call and its result. Providers reject a bare tool call,
// which would make the session impossible to continue, so every such call
// is paired with a synthetic error result before messages are converted to
// the wire format.

use std::collections::HashSet;

use crate::types::{AgentMessage, ContentBlock};

/// The body of the synthetic result standing in for a tool call whose
/// result never made it into the transcript.
const NO_RESULT_TEXT: &str = "No result provided";

/// Pair every unresolved tool call with a synthetic error result.
///
/// A call counts as resolved when a later tool result names its id;
/// otherwise a result reading "No result provided" is inserted where the
/// call's turn ends — before the next assistant or user message, or at the
/// end of the transcript. Clean transcripts pass through unchanged.
pub fn repair_tool_flow(messages: &[AgentMessage]) -> Vec<AgentMessage> {
    let mut result = Vec::with_capacity(messages.len());
    // Tool calls of the latest assistant turn, awaiting their results.
    let mut pending: Vec<(String, String)> = Vec::new();
    let mut resolved: HashSet<String> = HashSet::new();

    for msg in messages {
        match msg {
            AgentMessage::Assistant { content, .. } => {
                insert_synthetic_results(&mut result, &mut pending, &mut resolved);
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
            AgentMessage::Custom { .. } => result.push(msg.clone()),
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
            stop_reason: Some(StopReason::Stop),
            usage: Box::new(Usage::default()),
            error_message: None,
            timestamp: chrono::Utc::now(),
        }
    }

    fn tool_use(id: &str, name: &str) -> ContentBlock {
        ContentBlock::ToolUse {
            id: id.into(),
            name: name.into(),
            input: serde_json::json!({}),
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
    fn custom_messages_do_not_close_a_tool_turn() {
        let messages = vec![
            assistant(vec![tool_use("c1", "read")]),
            AgentMessage::Custom {
                custom_type: "note".into(),
                content: vec![],
                details: None,
                timestamp: chrono::Utc::now(),
            },
            AgentMessage::user("next"),
        ];
        let repaired = repair_tool_flow(&messages);
        // The synthetic result lands before the user message, after the
        // custom one — the custom message itself does not end the tool turn.
        assert_eq!(repaired.len(), 4);
        synthetic_at(&repaired, 2);
    }
}
