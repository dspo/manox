//! Subagent observation rows recovered from a restored transcript.
//!
//! Live rows are fed by `SubagentProgress` events, which die with the
//! process. Everything needed to rebuild them survives in the parent
//! transcript: the delegation tool call (definition name + prompt), its tool
//! result (the run id the row correlates on, plus the foreground envelope's
//! terminal state), and — for background runs — the peer-delivered
//! settlement report (author attribution + display text). Same
//! rebuild-from-history pattern as [`crate::plan::rebuild_from_messages`].

use crate::language_model::MessageContent;
use crate::message::{Message, MessageAuthor};
use crate::thread::ToolCallStatus;

/// One subagent rail row recovered from a restored transcript.
#[derive(Debug, Clone, PartialEq)]
pub struct RestoredSubagent {
    /// The runtime-minted run id (the rail row key; also the peer-delivery
    /// author a background run settles under).
    pub address: String,
    /// The definition name the subagent was dispatched from (the delegation
    /// tool's name).
    pub subagent_type: String,
    /// The Captain's dispatch prompt.
    pub prompt: String,
    /// Unix seconds of the dispatch turn: the timestamp of the parent
    /// message carrying the delegation tool call. The sub-agent panel's
    /// opening bubble header shows the send time, not the time the panel
    /// was opened.
    pub dispatched_at: i64,
    /// `Error` for a failure or timeout settlement; `Success` for a
    /// completion; `Cancelled` when the run ended without any settlement
    /// (quit-time kill and explicit abort are indistinguishable after the
    /// fact). Never `Running` on return.
    pub status: ToolCallStatus,
    /// The final answer, when one reached the parent.
    pub final_text: Option<String>,
}

/// Rebuild the subagent rows a restored thread's rail should show. Rows
/// follow dispatch order; a re-dispatch of the same run id replaces the row
/// in place (the live rail upserts by id). Dispatches whose tool result
/// errored before publishing a run id are dropped — a failed start never
/// surfaced in the rail.
pub fn rebuild_from_messages(messages: &[Message]) -> Vec<RestoredSubagent> {
    let mut rows: Vec<RestoredSubagent> = Vec::new();

    for m in messages {
        for c in &m.content {
            let MessageContent::ToolUse(tu) = c else {
                continue;
            };
            // The delegation shape: `description` + `prompt`, and none of
            // the retired Steer envelope keys.
            if tu.input.get("description").is_none()
                || tu.input.get("prompt").is_none()
                || tu.input.get("to").is_some()
                || tu.input.get("reason").is_some()
            {
                continue;
            }
            // The paired tool result carries the run id the row correlates
            // on (and, for a foreground run, the terminal state). A start
            // that failed has no run id — it never surfaced live, so it is
            // dropped.
            let Some(result_text) = result_for_tool_use(messages, &tu.id) else {
                continue;
            };
            let Some(run_id) = parse_run_id(&result_text) else {
                continue;
            };
            let prompt = tu
                .input
                .get("prompt")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let mut row = RestoredSubagent {
                address: run_id,
                subagent_type: tu.name.to_string(),
                prompt,
                dispatched_at: m.timestamp,
                status: ToolCallStatus::Running,
                final_text: None,
            };
            if let Some((status, output)) = parse_terminal_state(&result_text) {
                row.status = status;
                row.final_text = output;
            }
            match rows.iter_mut().find(|r| r.address == row.address) {
                Some(existing) => *existing = row,
                None => rows.push(row),
            }
        }
        // A peer delivery authored by a dispatched run id settles that
        // background row (foreground rows already settled off their tool
        // result). Deliveries from senders with no row (team members) are
        // ignored.
        if let Some(ui) = &m.ui
            && ui.peer
            && let Some(MessageAuthor::Agent(from)) = &ui.author
            && let Some(row) = rows.iter_mut().find(|r| r.address == *from)
            && row.status == ToolCallStatus::Running
        {
            let final_text = ui
                .display_text
                .clone()
                .unwrap_or_else(|| unwrap_peer_text(from, &text_of(m)));
            // A failed or timed-out run also delivers (unlike an abort): the
            // delivery prefixes the cause, and the live rail showed an Error
            // row.
            row.status = if final_text.starts_with(crate::subagent::SUBAGENT_FAILED_DELIVERY_PREFIX)
                || final_text.starts_with(crate::subagent::SUBAGENT_TIMED_OUT_DELIVERY_PREFIX)
            {
                ToolCallStatus::Error
            } else {
                ToolCallStatus::Success
            };
            row.final_text = Some(final_text);
        }
    }

    // No settlement anywhere: the run was terminated from outside (quit or
    // explicit abort) — settled as cancelled, not still running.
    for row in &mut rows {
        if row.status == ToolCallStatus::Running {
            row.status = ToolCallStatus::Cancelled;
        }
    }
    rows
}

/// The text of the (first) tool result paired with `tool_use_id`.
fn result_for_tool_use(messages: &[Message], tool_use_id: &str) -> Option<String> {
    for m in messages {
        for c in &m.content {
            if let MessageContent::ToolResult(r) = c
                && r.tool_use_id == tool_use_id
            {
                return Some(r.content.clone());
            }
        }
    }
    None
}

/// The `run_id` of a delegation tool result envelope.
fn parse_run_id(result_text: &str) -> Option<String> {
    let envelope: serde_json::Value = serde_json::from_str(result_text.trim()).ok()?;
    envelope
        .get("run_id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// A foreground result envelope settles its row inline: the stop reason maps
/// to the rail status and the output rides as the final text. A background
/// envelope carries no terminal state (the row stays open for the peer
/// delivery).
fn parse_terminal_state(result_text: &str) -> Option<(ToolCallStatus, Option<String>)> {
    let envelope: serde_json::Value = serde_json::from_str(result_text.trim()).ok()?;
    if envelope.get("kind").and_then(|v| v.as_str()) != Some("foreground") {
        return None;
    }
    let status = match envelope.get("stop_reason").and_then(|v| v.as_str()) {
        Some("completed") => ToolCallStatus::Success,
        Some("aborted") => ToolCallStatus::Cancelled,
        Some(_) => ToolCallStatus::Error,
        None => return None,
    };
    let output = envelope
        .get("output")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .filter(|s| !s.is_empty());
    Some((status, output))
}

/// The Text blocks of a message joined in order.
fn text_of(message: &Message) -> String {
    message
        .content
        .iter()
        .filter_map(|c| match c {
            MessageContent::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Strip the peer-delivery wrapper prefix when present. Sessions persisted
/// before the display-text sidecar only carry the wrapped model-facing form
/// (`[from {addr}]: …` / `[来自 {addr}]：…`, plus the render-failure fallback
/// `[from {addr}] …`); the body after the prefix is the delivered text.
fn unwrap_peer_text(from: &str, text: &str) -> String {
    let prefixes = [
        format!("[from {from}]: "),
        format!("[来自 {from}]："),
        format!("[from {from}] "),
    ];
    for prefix in &prefixes {
        if let Some(rest) = text.strip_prefix(prefix.as_str()) {
            return rest.to_string();
        }
    }
    text.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language_model::{LanguageModelToolResult, LanguageModelToolUse};
    use std::sync::Arc;

    fn message(content: Vec<MessageContent>) -> Message {
        Message::assistant(content)
    }

    fn tool_use(id: &str, name: &str, input: serde_json::Value) -> MessageContent {
        MessageContent::ToolUse(LanguageModelToolUse {
            id: id.to_string(),
            name: Arc::from(name),
            raw_input: input.to_string(),
            input,
            is_input_complete: true,
            thought_signature: None,
        })
    }

    fn tool_result(
        tool_use_id: &str,
        name: &str,
        is_error: bool,
        content: String,
    ) -> MessageContent {
        MessageContent::ToolResult(LanguageModelToolResult {
            tool_use_id: tool_use_id.to_string(),
            tool_name: Arc::from(name),
            is_error,
            content,
        })
    }

    fn foreground_result(tool_use_id: &str, run_id: &str, stop: &str) -> MessageContent {
        tool_result(
            tool_use_id,
            "Explore",
            stop != "completed",
            serde_json::json!({
                "kind": "foreground",
                "run_id": run_id,
                "stop_reason": stop,
                "output": "the answer",
            })
            .to_string(),
        )
    }

    fn background_result(tool_use_id: &str, run_id: &str) -> MessageContent {
        tool_result(
            tool_use_id,
            "Sailor",
            false,
            serde_json::json!({
                "kind": "background",
                "run_id": run_id,
                "status": "started",
            })
            .to_string(),
        )
    }

    fn peer(from: &str, text: &str) -> Message {
        let mut m = Message::user(text.to_string());
        m.ui = Some(MessageUiMetadata {
            peer: true,
            author: Some(MessageAuthor::Agent(from.to_string())),
            ..Default::default()
        });
        m
    }

    use crate::message::MessageAuthor;
    use crate::message::MessageUiMetadata;

    #[test]
    fn foreground_dispatch_settles_from_its_tool_result() {
        let messages = vec![message(vec![
            tool_use(
                "t1",
                "Explore",
                serde_json::json!({"description": "survey", "prompt": "find auth"}),
            ),
            foreground_result("t1", "sub-0", "completed"),
        ])];
        let rows = rebuild_from_messages(&messages);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].address, "sub-0");
        assert_eq!(rows[0].subagent_type, "Explore");
        assert_eq!(rows[0].prompt, "find auth");
        assert_eq!(rows[0].status, ToolCallStatus::Success);
        assert_eq!(rows[0].final_text.as_deref(), Some("the answer"));
    }

    #[test]
    fn foreground_failure_reads_error_status() {
        let messages = vec![message(vec![
            tool_use(
                "t1",
                "Sailor",
                serde_json::json!({"description": "w", "prompt": "p"}),
            ),
            foreground_result("t1", "sub-1", "error"),
        ])];
        let rows = rebuild_from_messages(&messages);
        assert_eq!(rows[0].status, ToolCallStatus::Error);
    }

    #[test]
    fn background_run_settles_via_peer_delivery_prefixes() {
        let messages = vec![
            message(vec![
                tool_use(
                    "t1",
                    "Sailor",
                    serde_json::json!({"description": "w", "prompt": "p"}),
                ),
                background_result("t1", "sub-2"),
            ]),
            // Legacy transcript shape: the peer text carries the wrapped
            // model-facing prefix, which the row strips.
            peer("sub-2", "[from sub-2] subagent failed: boom"),
        ];
        let rows = rebuild_from_messages(&messages);
        assert_eq!(rows[0].status, ToolCallStatus::Error);
        assert_eq!(rows[0].final_text.as_deref(), Some("subagent failed: boom"));
    }

    #[test]
    fn background_run_without_settlement_reads_cancelled() {
        let messages = vec![message(vec![
            tool_use(
                "t1",
                "Sailor",
                serde_json::json!({"description": "w", "prompt": "p"}),
            ),
            background_result("t1", "sub-3"),
        ])];
        let rows = rebuild_from_messages(&messages);
        assert_eq!(rows[0].status, ToolCallStatus::Cancelled);
    }

    /// A start failure (error result without a run id) never surfaced live,
    /// so no row is rebuilt.
    #[test]
    fn failed_start_produces_no_row() {
        let messages = vec![message(vec![
            tool_use(
                "t1",
                "Sailor",
                serde_json::json!({"description": "w", "prompt": "p"}),
            ),
            tool_result(
                "t1",
                "Sailor",
                true,
                "unknown subagent provider `ghost`".to_string(),
            ),
        ])];
        assert!(rebuild_from_messages(&messages).is_empty());
    }

    /// The retired Steer Dispatch shape is not a delegation call — old
    /// transcripts simply yield no rows (no compatibility reads).
    #[test]
    fn retired_steer_shape_is_ignored() {
        let messages = vec![message(vec![
            tool_use(
                "t1",
                "Steer",
                serde_json::json!({"to": {"agent_address": "e1", "spawn": "Explore"}, "reason": "Dispatch", "prompt": "x"}),
            ),
            tool_result("t1", "Steer", false, "delivered".to_string()),
        ])];
        assert!(rebuild_from_messages(&messages).is_empty());
    }

    /// A member-thread peer delivery (no dispatch row) never invents a row.
    #[test]
    fn peer_delivery_without_a_row_is_ignored() {
        let messages = vec![peer("member-thread", "hello from the member")];
        assert!(rebuild_from_messages(&messages).is_empty());
    }
}
