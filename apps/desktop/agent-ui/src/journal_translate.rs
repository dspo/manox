//! T6 §F.2 — the *positive* mapping module: journal wire entries → the
//! desktop's display items and live events.
//!
//! This is the successor of [`crate::server_note_translate`] for the v2
//! protocol: where that module reverse-projects a doomed `ServerNote` onto
//! `ThreadEvent`, this module projects an authoritative §C.2 journal row
//! forward onto the same surfaces — the display sequence (`HistoryEntry`,
//! what `ConversationState::rebuild_from_display` consumes) and the live
//! event stream (`ThreadEvent`, what the workspace handler consumes). The v1
//! note path stays wired through the dual-protocol window (§K.5); this module
//! is the v2 fold's translation table and the substrate T10 retires the v1
//! path onto.
//!
//! Shape notes (as-built §C.2):
//! - `message` rows carry `role` + kernel `ContentBlock` payloads (the
//!   `type`-tagged wire shape). `tool` / `custom` roles carry the *full*
//!   kernel `AgentMessage` JSON in `content[0]` (the wire stays opaque to
//!   non-transcript roles).
//! - `ui_note` rows carry `kind` + the persisted `UiNoteRecord` payload.
//! - `metrics{token_usage}` rows are transcript-adjacent usage sidecars
//!   (per-request usage keyed by the row's `messageId`); they have no
//!   display item of their own.

use manox_agent::ThreadEvent;
use manox_agent::db::{HistoryEntry, UiNoteKind, UiNoteRecord};
use manox_agent::language_model::{
    LanguageModelToolResult, LanguageModelToolUse, MessageContent, Role,
};
use manox_agent::message::{Message, MessageProvenance};
use manox_protocol::journal::{JournalWireEntry, JournalWireEvent};
use serde_json::Value;

/// Map one journal wire entry onto zero or more display items, in order.
/// Transcript rows (`message`, `ui_note`, `compaction`) produce items; the
/// rest of the vocabulary is state/lifecycle/diagnostic and renders through
/// projections or live events instead.
pub fn history_entries_of(entry: &JournalWireEntry) -> Vec<HistoryEntry> {
    match &entry.event {
        JournalWireEvent::Message { role, content, .. } => match role.as_str() {
            "user" => vec![HistoryEntry::Message(display_message(
                entry,
                Role::User,
                MessageProvenance::User,
                blocks_to_content(content),
            ))],
            "assistant" => {
                let blocks = blocks_to_content(content)
                    .into_iter()
                    .map(strip_plan_in_text)
                    .collect();
                vec![HistoryEntry::Message(display_message(
                    entry,
                    Role::Assistant,
                    MessageProvenance::Assistant,
                    blocks,
                ))]
            }
            // Tool / custom rows carry the full kernel AgentMessage JSON in
            // content[0] (wire-opaque, §C.2). Project onto the display the
            // way the kernel's own mirror does: a tool result rides a
            // user-role `ToolResult` block; a displayable custom row keeps
            // its blocks.
            "tool" => tool_row_to_entries(entry, content),
            "custom" => custom_row_to_entries(entry, content),
            other => {
                tracing::debug!(
                    role = other,
                    "journal display fold: unknown message role skipped"
                );
                Vec::new()
            }
        },
        JournalWireEvent::UiNote { data, .. } => {
            match serde_json::from_value::<UiNoteRecord>(data.clone()) {
                Ok(record) => vec![HistoryEntry::Note(record)],
                Err(err) => {
                    tracing::warn!(error = %err, "journal display fold: unparseable ui_note skipped");
                    Vec::new()
                }
            }
        }
        JournalWireEvent::Compaction {
            summary,
            retained_tail,
            ..
        } => {
            // The summary carrier heads the projection (the same text shape
            // the kernel's restore uses), followed by the retained tail.
            let mut out = vec![HistoryEntry::Message(display_message(
                entry,
                Role::User,
                MessageProvenance::User,
                vec![MessageContent::Text(format!(
                    "{}{summary}{}",
                    manox_harness::session::COMPACTION_SUMMARY_PREFIX,
                    manox_harness::session::COMPACTION_SUMMARY_SUFFIX
                ))],
            ))];
            for tail in retained_tail {
                let role = tail_role(tail);
                let pseudo = JournalWireEntry {
                    seq: entry.seq,
                    id: entry.id.clone(),
                    parent_id: entry.parent_id.clone(),
                    timestamp: entry.timestamp.clone(),
                    event: JournalWireEvent::Message {
                        role: role.clone(),
                        content: vec![tail.clone()],
                        usage: None,
                        origin_rpc: None,
                    },
                };
                match role.as_str() {
                    "user" | "assistant" => {
                        let blocks = blocks_of(tail.get("content"));
                        let blocks = if role == "assistant" {
                            blocks.into_iter().map(strip_plan_in_text).collect()
                        } else {
                            blocks
                        };
                        out.push(HistoryEntry::Message(display_message(
                            &pseudo,
                            if role == "assistant" {
                                Role::Assistant
                            } else {
                                Role::User
                            },
                            if role == "assistant" {
                                MessageProvenance::Assistant
                            } else {
                                MessageProvenance::User
                            },
                            blocks,
                        )));
                    }
                    "tool" => out.extend(tool_row_to_entries(&pseudo, std::slice::from_ref(tail))),
                    "custom" => {
                        out.extend(custom_row_to_entries(&pseudo, std::slice::from_ref(tail)))
                    }
                    _ => {}
                }
            }
            out
        }
        _ => Vec::new(),
    }
}

/// Project one journal wire entry onto the live `ThreadEvent` the
/// conversation renders — the §C.2 counterpart of
/// `server_note_translate::server_note_to_thread_event`. Transcript
/// persistence rows (`message`) return `None`: the durable user row retires
/// an echo through the store's echo map, and the transcript itself reaches
/// the view through the window rebuild; the assistant's live surface is the
/// delta rows below.
pub fn thread_event_of(entry: &JournalWireEntry) -> Option<ThreadEvent> {
    Some(match &entry.event {
        JournalWireEvent::AgentTextDelta { s } => ThreadEvent::AgentText(s.clone()),
        JournalWireEvent::AgentThinkingDelta { s } => ThreadEvent::AgentThinking(s.clone()),
        JournalWireEvent::ToolCall {
            call_id,
            name,
            title,
            status,
            input,
        } => ThreadEvent::ToolCall {
            id: call_id.clone(),
            name: name.clone(),
            title: title.clone(),
            status: parse_status(status),
            input: Some(input.clone()),
        },
        JournalWireEvent::ToolResult {
            call_id,
            output,
            is_error,
        } => ThreadEvent::ToolResult {
            id: call_id.clone(),
            output: output.clone(),
            is_error: *is_error,
        },
        JournalWireEvent::ToolOutputChunk { call_id, chunk } => ThreadEvent::ToolOutput {
            id: call_id.clone(),
            chunk: chunk.clone(),
        },
        JournalWireEvent::TurnStart => ThreadEvent::TurnStarted,
        JournalWireEvent::TurnFinish {
            cancelled,
            failed,
            stranded_steer_ids,
        } => ThreadEvent::TurnFinished {
            cancelled: *cancelled,
            failed: *failed,
            stranded_steer_ids: stranded_steer_ids.clone(),
        },
        JournalWireEvent::Stop { reason } => ThreadEvent::Stop(
            serde_json::from_value(Value::String(reason.clone().unwrap_or_default()))
                .unwrap_or(manox_agent::language_model::StopReason::EndTurn),
        ),
        JournalWireEvent::Retry {
            attempt,
            max_attempts,
            delay_secs,
            reason,
        } => ThreadEvent::Retry {
            attempt: *attempt,
            max_attempts: *max_attempts,
            delay_secs: *delay_secs,
            reason: reason.clone(),
            detail: None,
        },
        JournalWireEvent::Error { message } => ThreadEvent::Error(anyhow::anyhow!("{message}")),
        JournalWireEvent::ModelChange { from, to } => ThreadEvent::ModelChanged {
            from: from.as_ref().map(|m| m.0.clone()),
            to: to.0.clone(),
        },
        JournalWireEvent::CwdChange { path } => ThreadEvent::CwdChanged { path: path.clone() },
        JournalWireEvent::PermissionModeChange { mode } => ThreadEvent::PermissionModeChanged {
            mode: parse_permission_mode(mode),
        },
        JournalWireEvent::ReasoningEffortChange { effort } => ThreadEvent::ReasoningEffortChanged {
            effort: parse_reasoning_effort(effort),
        },
        JournalWireEvent::PlanModeChange { enabled } => {
            ThreadEvent::PlanModeChanged { enabled: *enabled }
        }
        JournalWireEvent::PlanUpdate { snapshot } => ThreadEvent::PlanUpdated {
            snapshot: serde_json::from_value(snapshot.clone()).ok()?,
        },
        JournalWireEvent::Goal { goal } => ThreadEvent::GoalChanged {
            goal: goal
                .as_ref()
                .and_then(|s| serde_json::from_value(s.clone()).ok()),
        },
        JournalWireEvent::BrowserSuites { suites } => ThreadEvent::BrowserSuitesChanged {
            suites: suites
                .iter()
                .filter_map(|s| serde_json::from_value(Value::String(s.clone())).ok())
                .collect(),
        },
        JournalWireEvent::BackgroundTask { snapshot } => ThreadEvent::BackgroundTaskUpdated {
            snapshot: serde_json::from_value(snapshot.clone()).ok()?,
        },
        JournalWireEvent::SubagentChild { event, .. } => ThreadEvent::SubagentChild {
            id: subagent_id_of(event).unwrap_or_default(),
            child: serde_json::from_value(event.clone()).ok()?,
        },
        JournalWireEvent::SubagentProgress {
            agent_id,
            agent_type,
            tool_uses,
            latest_activity,
            status,
        } => ThreadEvent::SubagentProgress {
            id: agent_id.clone(),
            subagent_type: agent_type.clone(),
            tool_uses: *tool_uses,
            token_usage: manox_agent::language_model::TokenUsage::default(),
            latest_activity: latest_activity.clone(),
            status: parse_status(status),
            health: None,
        },
        JournalWireEvent::CompactionStarted { tokens_before } => ThreadEvent::CompactionStarted {
            tokens_before: *tokens_before,
        },
        JournalWireEvent::Compaction {
            summary,
            tokens_before,
            retained_tail,
            ..
        } => ThreadEvent::Compaction {
            summary: summary.clone(),
            messages_compacted: retained_tail.len(),
            tokens_before: *tokens_before,
            retained_tail: retained_tail
                .iter()
                .filter_map(|v| serde_json::from_value(v.clone()).ok())
                .collect(),
        },
        // No live event: transcript persistence rows (echo/rebuild path),
        // state-change rows that ride the projection face (title, project,
        // pinned_archived, approval, …), tree bookkeeping and metrics.
        _ => return None,
    })
}

/// `metrics` rows carrying per-request usage: `(message_id, usage)` when the
/// row is a `token_usage` sidecar (the assistant-message usage projection the
/// store folds into `per_request_usage`).
pub fn usage_sidecar_of(entry: &JournalWireEntry) -> Option<(String, Value)> {
    match &entry.event {
        JournalWireEvent::Metrics { kind, data } if kind == "token_usage" => {
            let id = data.get("messageId").and_then(Value::as_str)?.to_string();
            Some((id, data.get("usage").cloned().unwrap_or(Value::Null)))
        }
        _ => None,
    }
}

/// The durable user-message echo key, if this row is a user `message` with
/// an `originRpc` (§F.2 echo retirement).
pub fn user_origin_rpc(entry: &JournalWireEntry) -> Option<&str> {
    match &entry.event {
        JournalWireEvent::Message {
            role, origin_rpc, ..
        } if role == "user" => origin_rpc.as_deref(),
        _ => None,
    }
}

fn display_message(
    entry: &JournalWireEntry,
    role: Role,
    provenance: MessageProvenance,
    content: Vec<MessageContent>,
) -> Message {
    Message {
        id: entry.id.clone(),
        timestamp: parse_ts_secs(&entry.timestamp),
        parent_id: entry.parent_id.clone(),
        provenance,
        role,
        content,
        ui: None,
    }
}

fn tool_row_to_entries(entry: &JournalWireEntry, content: &[Value]) -> Vec<HistoryEntry> {
    let Some(row) = content.first() else {
        return Vec::new();
    };
    // The kernel `AgentMessage::ToolResult` wire shape (camelCase, §C.2
    // kernel vocabulary): project onto a user-role `ToolResult` block, the
    // same display form the kernel mirror uses.
    if row.get("role").and_then(Value::as_str) == Some("toolResult") {
        let blocks = blocks_of(row.get("content"));
        let text = blocks
            .iter()
            .filter_map(MessageContent::to_str)
            .collect::<Vec<_>>()
            .join("");
        let block = MessageContent::ToolResult(LanguageModelToolResult {
            tool_use_id: row
                .get("toolCallId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            tool_name: row
                .get("toolName")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
                .into(),
            is_error: row.get("isError").and_then(Value::as_bool).unwrap_or(false),
            content: text,
        });
        return vec![HistoryEntry::Message(display_message(
            entry,
            Role::User,
            MessageProvenance::Tool,
            vec![block],
        ))];
    }
    // BashExecution and other kernel-extension rows: no stable display form
    // in the manox message vocabulary — skipped (L12 tolerance).
    Vec::new()
}

fn custom_row_to_entries(entry: &JournalWireEntry, content: &[Value]) -> Vec<HistoryEntry> {
    let Some(row) = content.first() else {
        return Vec::new();
    };
    if row.get("role").and_then(Value::as_str) != Some("custom") {
        return Vec::new();
    }
    if !row.get("display").and_then(Value::as_bool).unwrap_or(false) {
        return Vec::new();
    }
    vec![HistoryEntry::Message(display_message(
        entry,
        Role::Assistant,
        MessageProvenance::Assistant,
        blocks_of(row.get("content")),
    ))]
}

/// The kernel `ContentBlock` array (wire-opaque `type`-tagged rows) onto the
/// manox display blocks.
fn blocks_to_content(blocks: &[Value]) -> Vec<MessageContent> {
    blocks.iter().filter_map(block_to_content).collect()
}

fn blocks_of(value: Option<&Value>) -> Vec<MessageContent> {
    match value {
        Some(Value::Array(items)) => items.iter().filter_map(block_to_content).collect(),
        Some(Value::String(text)) => vec![MessageContent::Text(text.clone())],
        _ => Vec::new(),
    }
}

fn block_to_content(block: &Value) -> Option<MessageContent> {
    match block.get("type").and_then(Value::as_str)? {
        "text" => Some(MessageContent::Text(
            block.get("text").and_then(Value::as_str)?.to_string(),
        )),
        "thinking" => Some(MessageContent::Thinking {
            text: block
                .get("thinking")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            signature: block
                .get("thinkingSignature")
                .and_then(Value::as_str)
                .map(str::to_string),
        }),
        "image" => Some(MessageContent::Image {
            data: block
                .get("data")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            mime_type: block
                .get("mimeType")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        }),
        "toolCall" => Some(MessageContent::ToolUse(LanguageModelToolUse {
            id: block.get("id").and_then(Value::as_str)?.to_string(),
            name: block
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
                .into(),
            raw_input: block
                .get("arguments")
                .map(Value::to_string)
                .unwrap_or_default(),
            input: block.get("arguments").cloned().unwrap_or(Value::Null),
            is_input_complete: true,
            thought_signature: block
                .get("thoughtSignature")
                .and_then(Value::as_str)
                .map(str::to_string),
        })),
        // Unknown block kinds (L12 tolerance): the entry survives, the block
        // renders nothing.
        _ => None,
    }
}

fn strip_plan_in_text(block: MessageContent) -> MessageContent {
    match block {
        MessageContent::Text(text) => MessageContent::Text(
            manox_agent::proposed_plan::strip_proposed_plan_blocks(&text),
        ),
        other => other,
    }
}

/// The subagent handle of a `subagentChild` row's event payload (the kernel
/// event JSON may carry its own `id`; the row's `agentId` is authoritative
/// when the payload lacks one).
fn subagent_id_of(event: &Value) -> Option<String> {
    event.get("id").and_then(Value::as_str).map(str::to_string)
}

fn parse_ts_secs(ts: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(ts)
        .map(|d| d.timestamp())
        .unwrap_or(0)
}

/// The display role of a raw kernel `AgentMessage` JSON row (the `role` tag
/// is the camelCase kernel vocabulary: `user`/`assistant`/`toolResult`/…).
fn tail_role(row: &Value) -> String {
    match row.get("role").and_then(Value::as_str) {
        Some("toolResult") => "tool".into(),
        Some(other) => other.into(),
        None => String::new(),
    }
}

/// Reverse of the translate layer's kebab-case ToolCallStatus string.
fn parse_status(status: &str) -> manox_agent::thread::ToolCallStatus {
    serde_json::from_value(Value::String(status.to_string()))
        .unwrap_or(manox_agent::thread::ToolCallStatus::PendingApproval)
}

/// Reverse of the translate layer's permission-mode string.
fn parse_permission_mode(mode: &str) -> manox_agent::thread::PermissionMode {
    serde_json::from_value(Value::String(mode.to_string()))
        .unwrap_or(manox_agent::thread::PermissionMode::WorkspaceWrite)
}

/// Reverse of the translate layer's reasoning-effort string.
fn parse_reasoning_effort(effort: &str) -> manox_agent::language_model::ReasoningEffort {
    serde_json::from_value(Value::String(effort.to_string())).unwrap_or_default()
}

/// Silence a dead-code lint for the kind helper re-export path.
#[allow(dead_code)]
fn _kind_marker(_: UiNoteKind) {}

#[cfg(test)]
mod tests {
    use super::*;
    use manox_protocol::journal::UsagePayload;

    fn wire(seq: u64, event: JournalWireEvent) -> JournalWireEntry {
        JournalWireEntry {
            seq,
            id: format!("entry-{seq}"),
            parent_id: None,
            timestamp: "2026-09-04T00:00:00.000Z".to_string(),
            event,
        }
    }

    #[test]
    fn user_message_row_becomes_display_message() {
        let entry = wire(
            0,
            JournalWireEvent::Message {
                role: "user".into(),
                content: vec![serde_json::json!({"type": "text", "text": "hello"})],
                usage: None,
                origin_rpc: Some("rpc-1".into()),
            },
        );
        let items = history_entries_of(&entry);
        assert_eq!(items.len(), 1);
        let HistoryEntry::Message(m) = &items[0] else {
            panic!("expected a message item");
        };
        assert_eq!(m.role, Role::User);
        assert_eq!(m.id, "entry-0");
        assert!(m.timestamp > 0, "entry timestamp parses to unix seconds");
        assert_eq!(m.content.len(), 1);
        assert_eq!(user_origin_rpc(&entry), Some("rpc-1"));
    }

    #[test]
    fn assistant_message_row_maps_blocks() {
        let entry = wire(
            1,
            JournalWireEvent::Message {
                role: "assistant".into(),
                content: vec![
                    serde_json::json!({"type": "text", "text": "hi"}),
                    serde_json::json!({"type": "toolCall", "id": "tc1", "name": "Bash", "arguments": {"command": "ls"}}),
                ],
                usage: Some(UsagePayload {
                    input: 5,
                    output: 2,
                    cache_read: 0,
                    cache_write: 0,
                    reasoning: 0,
                }),
                origin_rpc: None,
            },
        );
        let items = history_entries_of(&entry);
        let HistoryEntry::Message(m) = &items[0] else {
            panic!("expected a message item");
        };
        assert_eq!(m.role, Role::Assistant);
        assert!(matches!(m.content[0], MessageContent::Text(ref t) if t == "hi"));
        assert!(matches!(m.content[1], MessageContent::ToolUse(_)));
        assert_eq!(user_origin_rpc(&entry), None);
    }

    #[test]
    fn tool_message_row_maps_to_tool_result_block() {
        let row = serde_json::json!({
            "role": "toolResult",
            "toolCallId": "tc1",
            "toolName": "Bash",
            "content": [{"type": "text", "text": "output!"}],
            "isError": false,
        });
        let entry = wire(
            2,
            JournalWireEvent::Message {
                role: "tool".into(),
                content: vec![row],
                usage: None,
                origin_rpc: None,
            },
        );
        let items = history_entries_of(&entry);
        let HistoryEntry::Message(m) = &items[0] else {
            panic!("expected a message item");
        };
        let MessageContent::ToolResult(r) = &m.content[0] else {
            panic!("expected a tool result block");
        };
        assert_eq!(r.tool_use_id, "tc1");
        assert_eq!(r.content, "output!");
    }

    #[test]
    fn ui_note_row_becomes_note_item() {
        let entry = wire(
            3,
            JournalWireEvent::UiNote {
                kind: "notice".into(),
                data: serde_json::json!({"kind": "notice", "data": {"text": "hello"}}),
            },
        );
        let items = history_entries_of(&entry);
        assert!(matches!(items[0], HistoryEntry::Note(_)));
    }

    #[test]
    fn compaction_row_emits_summary_plus_tail() {
        let tail = serde_json::json!({
            "role": "user",
            "content": [{"type": "text", "text": "kept"}],
            "timestamp": 1u64,
        });
        let entry = wire(
            4,
            JournalWireEvent::Compaction {
                summary: "folded".into(),
                messages_compacted: 1,
                tokens_before: 10,
                retained_tail: vec![tail],
                first_kept_entry_id: None,
            },
        );
        let items = history_entries_of(&entry);
        assert_eq!(items.len(), 2, "summary + retained tail");
        let HistoryEntry::Message(m) = &items[0] else {
            panic!("summary item expected");
        };
        let MessageContent::Text(t) = &m.content[0] else {
            panic!("summary text expected");
        };
        assert!(t.contains("folded"));
    }

    #[test]
    fn lifecycle_rows_map_to_thread_events() {
        type MapCheck = fn(&ThreadEvent) -> bool;
        let cases: Vec<(JournalWireEvent, MapCheck)> = vec![
            (
                JournalWireEvent::AgentTextDelta { s: "tok".into() },
                |e| matches!(e, ThreadEvent::AgentText(t) if t == "tok"),
            ),
            (JournalWireEvent::TurnStart, |e| {
                matches!(e, ThreadEvent::TurnStarted)
            }),
            (
                JournalWireEvent::TurnFinish {
                    cancelled: true,
                    failed: false,
                    stranded_steer_ids: vec![],
                },
                |e| {
                    matches!(
                        e,
                        ThreadEvent::TurnFinished {
                            cancelled: true,
                            ..
                        }
                    )
                },
            ),
            (
                JournalWireEvent::Error {
                    message: "boom".into(),
                },
                |e| matches!(e, ThreadEvent::Error(_)),
            ),
            (
                JournalWireEvent::CwdChange {
                    path: "/new".into(),
                },
                |e| matches!(e, ThreadEvent::CwdChanged { path } if path == "/new"),
            ),
        ];
        for (event, check) in cases {
            let ev = thread_event_of(&wire(9, event));
            assert!(
                ev.as_ref().is_some_and(check),
                "event mapping failed: {ev:?}"
            );
        }
    }

    #[test]
    fn transcript_rows_have_no_live_event() {
        let message = wire(
            5,
            JournalWireEvent::Message {
                role: "user".into(),
                content: vec![],
                usage: None,
                origin_rpc: None,
            },
        );
        assert!(thread_event_of(&message).is_none());
        let title = wire(6, JournalWireEvent::Title { title: "t".into() });
        assert!(thread_event_of(&title).is_none());
    }

    #[test]
    fn usage_sidecar_row_extracts_message_key() {
        let entry = wire(
            7,
            JournalWireEvent::Metrics {
                kind: "token_usage".into(),
                data: serde_json::json!({"messageId": "m-1", "usage": {"input": 3}}),
            },
        );
        let (id, usage) = usage_sidecar_of(&entry).expect("usage sidecar");
        assert_eq!(id, "m-1");
        assert_eq!(usage["input"], 3);
    }
}
