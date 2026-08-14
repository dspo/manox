//! Mapping from `ThreadEvent` to a JSON string for the TypeScript host.
//!
//! `ThreadEvent` is not serde-serializable as a whole (it carries `Entity`,
//! `anyhow::Error`, and other non-`Serialize` payloads), so the P0 host
//! renderers get an explicit projection. Unknown variants yield `None` and
//! are skipped by the caller.

use agent::{ThreadEvent, ToolCallStatus};
use serde_json::{Value, json};

fn status_str(status: &ToolCallStatus) -> Value {
    serde_json::to_value(status).unwrap_or(Value::Null)
}

/// Project a `ThreadEvent` into the JSON shape consumed by the P0 TS host.
pub fn thread_event_to_json(ev: &ThreadEvent) -> Option<String> {
    let v = match ev {
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
    Some(v.to_string())
}
