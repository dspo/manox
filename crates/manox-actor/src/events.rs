//! Mapping from `ThreadEvent` to JSON strings for the TypeScript host.
//!
//! `ThreadEvent` is not serde-serializable as a whole (it carries `Entity`,
//! `anyhow::Error`, and other non-`Serialize` payloads), so the host
//! renderers get an explicit projection. Unknown variants yield `None` and
//! are skipped by the caller.
//!
//! Variants intentionally not projected yet (silently dropped; surface them
//! here when the host grows the matching UI): `SubagentStarted`,
//! `SubagentProgress`, `SubagentChild`, `ApprovalDecision`, `Retry`,
//! `PrefixStability`, `CacheInvalidation`, `SideCallMetricsUpdated`,
//! `MainCallMetricsUpdated`, `ReasoningEffortChanged`, `GoalChanged`,
//! `WorktreeChanged`, `CompactionStarted`, `Compaction`, `PlanReady`,
//! `PlanUpdated`, `PlanModeChanged`, `PeerMessage`, `SteerInjected`,
//! `BrowserNotification`, `InboundAuthorization`, `BackgroundTaskUpdated`,
//! `HistoryProgress`, `HistoryRestored`.

use agent::{ThreadEvent, ToolCallStatus};
use serde_json::{Value, json};

fn status_str(status: &ToolCallStatus) -> Value {
    serde_json::to_value(status).unwrap_or(Value::Null)
}

/// Project a `ThreadEvent` into the JSON payload consumed by the TS host.
/// Every projected event carries the owning session's id so multiple host
/// surfaces can share one actor without cross-talk.
pub fn thread_event_to_json(ev: &ThreadEvent, session_id: Option<&str>) -> Option<String> {
    let mut v = match ev {
        ThreadEvent::AgentText(text) => json!({"type": "agent_text", "text": text}),
        ThreadEvent::AgentThinking(text) => json!({"type": "agent_thinking", "text": text}),
        ThreadEvent::ToolCall {
            id,
            name,
            title,
            status,
            input,
        } => json!({
            "type": "tool_call",
            "id": id,
            "name": name,
            "title": title,
            "status": status_str(status),
            "input": input,
        }),
        ThreadEvent::ToolResult {
            id,
            output,
            is_error,
        } => json!({"type": "tool_result", "id": id, "output": output, "is_error": is_error}),
        ThreadEvent::ToolOutput { id, chunk } => {
            json!({"type": "tool_output", "id": id, "chunk": chunk})
        }
        ThreadEvent::TurnStarted => json!({"type": "turn_started"}),
        ThreadEvent::Stop(reason) => {
            json!({"type": "stop", "reason": serde_json::to_value(reason).unwrap_or(Value::Null)})
        }
        ThreadEvent::ToolCallAuthorization {
            id,
            tool_name,
            summary,
            input,
        } => json!({
            "type": "tool_call_authorization",
            "id": id,
            "tool_name": tool_name,
            "summary": summary,
            "input": input,
        }),
        ThreadEvent::TurnFinished {
            cancelled, failed, ..
        } => json!({"type": "turn_finished", "cancelled": cancelled, "failed": failed}),
        ThreadEvent::ModelChanged { from, to } => {
            json!({"type": "model_changed", "from": from, "to": to})
        }
        ThreadEvent::ApprovalModeChanged { mode } => {
            json!({"type": "approval_mode_changed", "mode": mode})
        }
        ThreadEvent::Error(e) => json!({"type": "error", "message": e.to_string()}),
        ThreadEvent::TokenUsageUpdated(u) => json!({
            "type": "token_usage",
            "input": u.input_tokens,
            "output": u.output_tokens,
            "cache_creation": u.cache_creation_input_tokens,
            "cache_read": u.cache_read_input_tokens,
        }),
        _ => return None,
    };
    if let Some(id) = session_id {
        if let Some(obj) = v.as_object_mut() {
            obj.insert("sessionId".to_string(), Value::String(id.to_string()));
        }
    }
    Some(v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projects_session_id_into_every_event() {
        let json = thread_event_to_json(&ThreadEvent::TurnStarted, Some("s1")).unwrap();
        let v: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "turn_started");
        assert_eq!(v["sessionId"], "s1");
    }

    #[test]
    fn projects_model_changed() {
        let json = thread_event_to_json(
            &ThreadEvent::ModelChanged {
                from: Some("a".into()),
                to: "b".into(),
            },
            Some("s1"),
        )
        .unwrap();
        let v: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "model_changed");
        assert_eq!(v["from"], "a");
        assert_eq!(v["to"], "b");
    }

    #[test]
    fn drops_unprojected_variants() {
        // HistoryRestored is on the documented drop list.
        assert!(thread_event_to_json(&ThreadEvent::HistoryRestored, Some("s1")).is_none());
    }
}
