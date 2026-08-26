//! Subagent observation rows recovered from a restored transcript.
//!
//! Live rows are fed by `SubagentProgress` events, which die with the
//! process. Everything needed to rebuild them survives in the parent
//! transcript: the Steer Dispatch tool call (address / type / prompt), its
//! tool result (a failed spawn never surfaced in the rail), and the
//! peer-delivered completion (author attribution + display text). Same
//! rebuild-from-history pattern as [`crate::plan::rebuild_from_messages`].

use std::collections::HashSet;

use crate::language_model::MessageContent;
use crate::message::{Message, MessageAuthor};
use crate::thread::ToolCallStatus;

/// The spawn type that creates a real member thread, not an in-thread
/// subagent coroutine — member dispatches never enter the subagent rail.
const TEAM_MEMBER_SPAWN: &str = "TeamMember";

/// One subagent rail row recovered from a restored transcript.
#[derive(Debug, Clone, PartialEq)]
pub struct RestoredSubagent {
    /// The caller-chosen subagent address (the rail row key).
    pub address: String,
    /// The capability definition name the subagent was spawned from.
    pub subagent_type: String,
    /// The Captain's dispatch prompt.
    pub prompt: String,
    /// `Error` for a failure or timeout delivery; `Success` for a completion
    /// delivery; `Cancelled` when the run ended without any delivery
    /// (quit-time kill and explicit abort are indistinguishable after the
    /// fact). Never `Running` on return.
    pub status: ToolCallStatus,
    /// The delivered final answer, when one reached the parent.
    pub final_text: Option<String>,
}

/// Rebuild the subagent rows a restored thread's rail should show. Rows
/// follow dispatch order; a re-dispatch of the same address replaces the row
/// in place (the live rail upserts by address). Dispatches whose Steer tool
/// result errored are dropped — a failed spawn never surfaced live.
pub fn rebuild_from_messages(messages: &[Message]) -> Vec<RestoredSubagent> {
    let mut errored: HashSet<&str> = HashSet::new();
    for m in messages {
        for c in &m.content {
            if let MessageContent::ToolResult(r) = c
                && r.is_error
            {
                errored.insert(r.tool_use_id.as_str());
            }
        }
    }

    let mut rows: Vec<RestoredSubagent> = Vec::new();
    for m in messages {
        for c in &m.content {
            if let MessageContent::ToolUse(tu) = c
                && tu.name.as_ref() == "Steer"
                && !errored.contains(tu.id.as_str())
                && let Some(row) = parse_dispatch(&tu.input)
            {
                match rows.iter_mut().find(|r| r.address == row.address) {
                    Some(existing) => *existing = row,
                    None => rows.push(row),
                }
            }
        }
        // A peer delivery authored by a dispatched address settles that
        // subagent's row. Deliveries from senders with no dispatch row
        // (team members) are ignored.
        if let Some(ui) = &m.ui
            && ui.peer
            && let Some(MessageAuthor::Agent(from)) = &ui.author
            && let Some(row) = rows.iter_mut().find(|r| r.address == *from)
        {
            let final_text = ui
                .display_text
                .clone()
                .unwrap_or_else(|| unwrap_peer_text(from, &text_of(m)));
            // A failed or timed-out run also delivers (unlike an abort): the
            // bus prefixes the report, and the live rail showed an Error row.
            row.status = if final_text
                .starts_with(crate::steer_bus::SUBAGENT_FAILED_DELIVERY_PREFIX)
                || final_text.starts_with(crate::steer_bus::SUBAGENT_TIMED_OUT_DELIVERY_PREFIX)
            {
                ToolCallStatus::Error
            } else {
                ToolCallStatus::Success
            };
            row.final_text = Some(final_text);
        }
    }

    // No completion delivered: the run was terminated from outside (quit
    // or explicit abort) — settled, not still running.
    for row in &mut rows {
        if row.status == ToolCallStatus::Running {
            row.status = ToolCallStatus::Cancelled;
        }
    }
    rows
}

/// One subagent dispatch a Steer tool call describes, when it is one: a
/// `Dispatch` whose `to.spawn` names a capability definition (`TeamMember`
/// spawns a real thread and is not a subagent). Inject/Abort carry no spawn.
fn parse_dispatch(input: &serde_json::Value) -> Option<RestoredSubagent> {
    if input.get("reason").and_then(|v| v.as_str()) != Some("Dispatch") {
        return None;
    }
    let to = input.get("to")?;
    let spawn = to.get("spawn").and_then(|v| v.as_str())?;
    if spawn == TEAM_MEMBER_SPAWN {
        return None;
    }
    let address = to
        .get("agent_address")
        .and_then(|v| v.as_str())?
        .to_string();
    let prompt = input
        .get("prompt")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    Some(RestoredSubagent {
        address,
        subagent_type: spawn.to_string(),
        prompt,
        status: ToolCallStatus::Running,
        final_text: None,
    })
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
    use crate::message::MessageUiMetadata;
    use std::sync::Arc;

    fn steer_use(id: &str, addr: &str, spawn: &str, reason: &str, prompt: &str) -> Message {
        let input = serde_json::json!({
            "to": { "agent_address": addr, "spawn": spawn },
            "reason": reason,
            "prompt": prompt,
        });
        Message::assistant(vec![MessageContent::ToolUse(LanguageModelToolUse {
            id: id.into(),
            name: Arc::from("Steer"),
            raw_input: input.to_string(),
            input,
            is_input_complete: true,
            thought_signature: None,
        })])
    }

    fn tool_result(tool_use_id: &str, is_error: bool, content: &str) -> Message {
        Message::user_with_content(vec![MessageContent::ToolResult(LanguageModelToolResult {
            tool_use_id: tool_use_id.into(),
            tool_name: "Steer".into(),
            is_error,
            content: content.into(),
        })])
    }

    fn delivery(from: &str, body: &str, display_text: Option<&str>) -> Message {
        let mut m = Message::user(format!("[from {from}]: {body}"));
        m.ui = Some(MessageUiMetadata {
            author: Some(MessageAuthor::Agent(from.to_string())),
            peer: true,
            display_text: display_text.map(|s| s.to_string()),
            ..Default::default()
        });
        m
    }

    #[test]
    fn dispatch_then_delivery_restores_success() {
        let messages = vec![
            steer_use("tu_1", "worker", "Sailor", "Dispatch", "fix the bug"),
            tool_result("tu_1", false, "dispatched"),
            delivery("worker", "all done", Some("all done")),
        ];
        let rows = rebuild_from_messages(&messages);
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.address, "worker");
        assert_eq!(row.subagent_type, "Sailor");
        assert_eq!(row.prompt, "fix the bug");
        assert_eq!(row.status, ToolCallStatus::Success);
        assert_eq!(row.final_text.as_deref(), Some("all done"));
    }

    #[test]
    fn failed_run_delivery_restores_error_not_success() {
        let failure = format!(
            "{}model exploded",
            crate::steer_bus::SUBAGENT_FAILED_DELIVERY_PREFIX
        );
        let messages = vec![
            steer_use("tu_1", "worker", "Sailor", "Dispatch", "do it"),
            tool_result("tu_1", false, "dispatched"),
            delivery("worker", &failure, Some(&failure)),
        ];
        let rows = rebuild_from_messages(&messages);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, ToolCallStatus::Error);
        assert_eq!(rows[0].final_text.as_deref(), Some(failure.as_str()));
    }

    #[test]
    fn timed_out_run_delivery_restores_error_not_success() {
        let timeout = format!(
            "{}budget 1000ms exceeded after 1.2s (2 turns, 3 tool calls)",
            crate::steer_bus::SUBAGENT_TIMED_OUT_DELIVERY_PREFIX
        );
        let messages = vec![
            steer_use("tu_1", "worker", "Explore", "Dispatch", "find it"),
            tool_result("tu_1", false, "dispatched"),
            delivery("worker", &timeout, Some(&timeout)),
        ];
        let rows = rebuild_from_messages(&messages);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, ToolCallStatus::Error);
        assert_eq!(rows[0].final_text.as_deref(), Some(timeout.as_str()));
    }

    #[test]
    fn dispatch_without_delivery_restores_cancelled() {
        let messages = vec![
            steer_use("tu_1", "worker", "Explore", "Dispatch", "find it"),
            tool_result("tu_1", false, "dispatched"),
        ];
        let rows = rebuild_from_messages(&messages);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, ToolCallStatus::Cancelled);
        assert!(rows[0].final_text.is_none());
    }

    #[test]
    fn errored_dispatch_is_dropped() {
        let messages = vec![
            steer_use("tu_1", "worker", "Sailor", "Dispatch", "do it"),
            tool_result("tu_1", true, "agent worker already exists"),
        ];
        assert!(rebuild_from_messages(&messages).is_empty());
    }

    #[test]
    fn team_member_spawn_is_excluded() {
        let messages = vec![
            steer_use("tu_1", "member-1", "TeamMember", "Dispatch", "join"),
            tool_result("tu_1", false, "spawned"),
        ];
        assert!(rebuild_from_messages(&messages).is_empty());
    }

    #[test]
    fn inject_and_abort_do_not_create_rows() {
        let messages = vec![
            steer_use("tu_1", "worker", "Sailor", "Dispatch", "do it"),
            tool_result("tu_1", false, "dispatched"),
            steer_use("tu_2", "worker", "Sailor", "Inject", "also this"),
            tool_result("tu_2", false, "injected"),
            steer_use("tu_3", "worker", "Sailor", "Abort", ""),
            tool_result("tu_3", false, "aborted"),
        ];
        let rows = rebuild_from_messages(&messages);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].prompt, "do it",
            "Inject must not replace the dispatch prompt"
        );
    }

    #[test]
    fn redispatch_of_same_address_is_last_wins() {
        let messages = vec![
            steer_use("tu_1", "worker", "Sailor", "Dispatch", "first run"),
            tool_result("tu_1", false, "dispatched"),
            delivery("worker", "first result", Some("first result")),
            steer_use("tu_2", "worker", "Sailor", "Dispatch", "second run"),
            tool_result("tu_2", false, "dispatched"),
        ];
        let rows = rebuild_from_messages(&messages);
        assert_eq!(rows.len(), 1, "one row per address");
        assert_eq!(rows[0].prompt, "second run");
        assert_eq!(rows[0].status, ToolCallStatus::Cancelled);
        assert!(
            rows[0].final_text.is_none(),
            "the first run's delivery belongs to the replaced row"
        );
    }

    #[test]
    fn peer_delivery_without_dispatch_is_ignored() {
        let messages = vec![delivery("member-a", "team chatter", None)];
        assert!(rebuild_from_messages(&messages).is_empty());
    }

    #[test]
    fn missing_display_text_unwraps_wrapper_prefixes() {
        for (from, text, expected) in [
            ("w", "[from w]: done", "done"),
            ("w", "[来自 w]：完成", "完成"),
            ("w", "[from w] fallback body", "fallback body"),
            ("w", "no wrapper at all", "no wrapper at all"),
            // A different sender's wrapper is not stripped.
            ("w", "[from other]: x", "[from other]: x"),
        ] {
            assert_eq!(unwrap_peer_text(from, text), expected, "input: {text}");
        }
    }

    #[test]
    fn delivery_without_display_text_falls_back_to_unwrapped_body() {
        let messages = vec![
            steer_use("tu_1", "w", "Sailor", "Dispatch", "go"),
            tool_result("tu_1", false, "dispatched"),
            delivery("w", "legacy body", None),
        ];
        let rows = rebuild_from_messages(&messages);
        assert_eq!(rows[0].final_text.as_deref(), Some("legacy body"));
    }

    #[test]
    fn empty_transcript_restores_nothing() {
        assert!(rebuild_from_messages(&[]).is_empty());
    }
}
