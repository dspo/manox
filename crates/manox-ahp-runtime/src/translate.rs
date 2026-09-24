//! The kernel → journal vocabulary projection.
//!
//! One session's `SessionTreeEntry` rows become the §C.2 wire events the AHP
//! fold, the v2 follow stream and the cold reader all consume. This is the
//! **disk** vocabulary's projection, and it is total: every kernel entry maps to
//! exactly one event, so a new variant cannot be added on the kernel side
//! without this file failing to compile — which is the moment the AHP mapping
//! table (and the v2 one, while it lives) must state where it belongs.
//!
//! The v2 *adjudication* translation (`ThreadEvent` → `ServerCall`) lives in the
//! gateway, not here: it names the retiring protocol's vocabulary, and only the
//! v2 pump consumes it.

use manox_harness::session::SessionTreeEntry;
use manox_harness::types::AgentMessage;
use manox_journal::{JournalWireEntry, JournalWireEvent, ModelRef, UsagePayload};

pub fn wire_entry(seq: u64, entry: &SessionTreeEntry) -> Option<JournalWireEntry> {
    Some(JournalWireEntry {
        seq,
        id: entry.id().to_string(),
        parent_id: entry.parent_id().map(str::to_string),
        timestamp: entry
            .timestamp()
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        event: wire_event(entry)?,
    })
}

/// The §C.2 event projection of one kernel entry — total (see the module
/// comment); the `Option` is signature stability, never a skip.
pub fn wire_event(entry: &SessionTreeEntry) -> Option<JournalWireEvent> {
    use JournalWireEvent as W;
    Some(match entry {
        // ── transcript ──────────────────────────────────────────────────
        SessionTreeEntry::Message {
            message, origin, ..
        } => match message {
            AgentMessage::User { content, .. } => W::Message {
                role: "user".into(),
                content: content_blocks(content),
                usage: None,
                // The kernel pins the Submit's RPC id on exactly this entry
                // (T5b pending-origin middleware drain) — the echo-retirement
                // correlation travels on the durable row itself (§F.2).
                origin_rpc: origin.clone(),
                display: None,
            },
            AgentMessage::Assistant { content, usage, .. } => W::Message {
                role: "assistant".into(),
                content: content_blocks(content),
                usage: Some(UsagePayload {
                    input: usage.input_tokens,
                    output: usage.output_tokens,
                    cache_read: usage.cache_read_input_tokens,
                    cache_write: usage.cache_creation_input_tokens,
                    reasoning: usage.reasoning_tokens.unwrap_or(0),
                }),
                origin_rpc: None,
                display: None,
            },
            AgentMessage::ToolResult { .. } | AgentMessage::BashExecution { .. } => W::Message {
                role: "tool".into(),
                content: message_value(message),
                usage: None,
                origin_rpc: None,
                display: None,
            },
            // Kernel-extension message roles ride the generic transcript row
            // with the kernel JSON shape verbatim (wire-opaque, §C.2).
            AgentMessage::Custom { display, .. } => W::Message {
                role: "custom".into(),
                content: message_value(message),
                usage: None,
                origin_rpc: None,
                // The kernel's UI-visibility flag rides the wire so clients
                // can filter hidden rows (the model context is unaffected —
                // the projection includes them either way).
                display: Some(*display),
            },
        },
        SessionTreeEntry::UiNote { note, .. } => W::UiNote {
            kind: note
                .get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or("notice")
                .to_string(),
            data: note.clone(),
        },
        // ── lifecycle ───────────────────────────────────────────────────
        SessionTreeEntry::TurnStart { .. } => W::TurnStart,
        SessionTreeEntry::TurnFinish {
            cancelled,
            failed,
            stranded_steer_ids,
            ..
        } => W::TurnFinish {
            cancelled: *cancelled,
            failed: *failed,
            stranded_steer_ids: stranded_steer_ids.clone(),
        },
        SessionTreeEntry::Stop { reason, .. } => W::Stop {
            reason: reason.clone(),
        },
        SessionTreeEntry::Retry {
            attempt,
            max_attempts,
            delay_secs,
            reason,
            detail,
            ..
        } => W::Retry {
            attempt: *attempt,
            max_attempts: *max_attempts,
            delay_secs: *delay_secs,
            // The kernel's diagnostic `detail` folds into the wire reason;
            // the §C.2 retry row carries one string.
            reason: match detail {
                Some(d) if !d.is_empty() => format!("{reason}: {d}"),
                _ => reason.clone(),
            },
        },
        SessionTreeEntry::ErrorEvent { message, .. } => W::Error {
            message: message.clone(),
        },
        // ── streaming delta ─────────────────────────────────────────────
        SessionTreeEntry::AgentTextDelta { delta, .. } => W::AgentTextDelta { s: delta.clone() },
        SessionTreeEntry::AgentThinkingDelta { delta, .. } => {
            W::AgentThinkingDelta { s: delta.clone() }
        }
        SessionTreeEntry::ToolCall {
            call_id,
            name,
            title,
            status,
            input,
            ..
        } => W::ToolCall {
            call_id: call_id.clone(),
            name: name.clone(),
            title: title.clone(),
            status: status.clone(),
            input: input.clone().unwrap_or(serde_json::Value::Null),
        },
        SessionTreeEntry::ToolResult {
            call_id,
            output,
            is_error,
            ..
        } => W::ToolResult {
            call_id: call_id.clone(),
            output: output.clone(),
            is_error: *is_error,
        },
        SessionTreeEntry::ToolOutputChunk { call_id, chunk, .. } => W::ToolOutputChunk {
            call_id: call_id.clone(),
            chunk: chunk.clone(),
        },
        SessionTreeEntry::SubagentChild {
            agent_id, event, ..
        } => W::SubagentChild {
            agent_id: agent_id.clone(),
            event: event.clone(),
        },
        SessionTreeEntry::SubagentProgress {
            agent_id,
            agent_type,
            tool_uses,
            latest_activity,
            status,
            ..
        } => W::SubagentProgress {
            agent_id: agent_id.clone(),
            agent_type: agent_type.clone(),
            tool_uses: *tool_uses,
            latest_activity: latest_activity.clone(),
            status: status.clone(),
        },
        // ── state change ────────────────────────────────────────────────
        SessionTreeEntry::ModelChange {
            provider, model_id, ..
        } => W::ModelChange {
            from: None,
            // Canonical `{provider}/{model}` on the wire (L8).
            to: ModelRef::new(format!("{provider}/{model_id}")),
        },
        SessionTreeEntry::CwdChange { cwd, .. } => W::CwdChange { path: cwd.clone() },
        // The kernel records the reasoning tier as a thinking level; the
        // §C.2 row names it `reasoningEffortChange`.
        SessionTreeEntry::ThinkingLevelChange { thinking_level, .. } => W::ReasoningEffortChange {
            effort: thinking_level.clone(),
        },
        SessionTreeEntry::ProjectChange { path, .. } => W::ProjectChange { path: path.clone() },
        SessionTreeEntry::PermissionModeChange { mode, .. } => {
            W::PermissionModeChange { mode: mode.clone() }
        }
        SessionTreeEntry::PlanModeRequest { enabled, .. } => {
            W::PlanModeRequest { enabled: *enabled }
        }
        SessionTreeEntry::PlanModeChange { enabled, .. } => W::PlanModeChange { enabled: *enabled },
        SessionTreeEntry::PlanUpdate { snapshot, .. } => W::PlanUpdate {
            snapshot: snapshot.clone(),
        },
        SessionTreeEntry::PlanReview {
            state, plan_file, ..
        } => W::PlanReview {
            state: state.clone(),
            plan_file: plan_file.clone(),
        },
        SessionTreeEntry::Goal { goal, .. } => W::Goal { goal: goal.clone() },
        SessionTreeEntry::Title { title, .. } => W::Title {
            title: title.clone(),
        },
        SessionTreeEntry::BrowserSuites { suites, .. } => W::BrowserSuites {
            suites: suites.clone(),
        },
        SessionTreeEntry::BackgroundTask { snapshot, .. } => W::BackgroundTask {
            snapshot: snapshot.clone(),
        },
        SessionTreeEntry::Approval {
            kind,
            auth_id,
            payload,
            ..
        } => W::Approval {
            kind: kind.clone(),
            auth_id: auth_id.clone(),
            tool_name: payload
                .get("toolName")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            tool_call_id: payload
                .get("toolCallId")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            verdict: payload
                .get("verdict")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            reason: payload
                .get("reason")
                .and_then(|v| v.as_str())
                .map(str::to_string),
        },
        SessionTreeEntry::Question {
            kind,
            auth_id,
            payload,
            ..
        } => W::Question {
            kind: kind.clone(),
            auth_id: auth_id.clone(),
            tool_name: payload
                .get("toolName")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            tool_call_id: payload
                .get("toolCallId")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            verdict: payload
                .get("verdict")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            reason: payload
                .get("reason")
                .and_then(|v| v.as_str())
                .map(str::to_string),
        },
        SessionTreeEntry::PinnedArchived {
            pinned, archived, ..
        } => W::PinnedArchived {
            pinned: *pinned,
            archived: *archived,
        },
        // ── compaction / tree ───────────────────────────────────────────
        SessionTreeEntry::Compaction {
            summary,
            tokens_before,
            first_kept_entry_id,
            retained_tail,
            ..
        } => W::Compaction {
            summary: summary.clone(),
            // The kernel stores the retained tail but not the compacted
            // count / pre-boundary tokens as separate rows; derive the
            // count from the tail's absence semantics: 0 until the kernel
            // stamps it (§C.2 payload best-effort, T5 fold source).
            messages_compacted: 0,
            tokens_before: *tokens_before,
            retained_tail: retained_tail
                .clone()
                .unwrap_or_default()
                .into_iter()
                .filter_map(|m| serde_json::to_value(m).ok())
                .collect(),
            first_kept_entry_id: first_kept_entry_id.clone(),
        },
        SessionTreeEntry::CompactionStarted { tokens_before, .. } => W::CompactionStarted {
            tokens_before: *tokens_before,
        },
        SessionTreeEntry::BranchSummary { summary, .. } => W::BranchSummary {
            text: summary.clone(),
        },
        SessionTreeEntry::Label { label, .. } => W::Label {
            label: label.clone().unwrap_or_default(),
        },
        SessionTreeEntry::SessionInfo { name, .. } => W::SessionInfo {
            data: serde_json::json!({ "name": name }),
        },
        SessionTreeEntry::Leaf { target_id, .. } => W::Leaf {
            // A `None` target resets the leaf to the root; the §C.2 row
            // names the target id (empty = root reset).
            target_id: target_id.clone().unwrap_or_default(),
        },
        // ── metrics ─────────────────────────────────────────────────────
        SessionTreeEntry::Metrics {
            metric_type, data, ..
        } => W::Metrics {
            kind: metric_type.clone(),
            data: data.clone(),
        },
        // ── kernel rows with no §C.2 wire vocabulary ────────────────────
        SessionTreeEntry::ActiveToolsChange {
            active_tool_names, ..
        } => W::ActiveToolsChange {
            tools: active_tool_names.clone(),
        },
        // Wire-opaque extension rows (§C.2): carried verbatim so the journal's
        // wire page stays seq-dense (a wire-less kind would break the client
        // fold's `assertPage` adjacency and loop snapshot → Resync).
        SessionTreeEntry::Custom {
            custom_type, data, ..
        } => W::Custom {
            custom_type: custom_type.clone(),
            data: data.clone().unwrap_or(serde_json::Value::Null),
        },
        SessionTreeEntry::CustomMessage {
            custom_type,
            content,
            display,
            ..
        } => W::CustomMessage {
            custom_type: custom_type.clone(),
            content: content_blocks(content),
            display: *display,
        },
    })
}

/// Kernel content blocks pass through the wire opaque (§C.2 "storage shape");
/// an unspecifiable block is dropped rather than failing the frame.
fn content_blocks(blocks: &[manox_harness::types::ContentBlock]) -> Vec<serde_json::Value> {
    blocks
        .iter()
        .filter_map(|b| {
            serde_json::to_value(b)
                .map_err(|e| {
                    tracing::warn!(error = %e, "content block dropped (serialization failure)");
                })
                .ok()
        })
        .collect()
}

/// The full kernel JSON object of a non-transcript message (wire-opaque).
fn message_value(message: &AgentMessage) -> Vec<serde_json::Value> {
    match serde_json::to_value(message) {
        Ok(v) => vec![v],
        Err(e) => {
            tracing::error!(error = %e, "message serialization failed");
            vec![serde_json::json!({
                "type": "error",
                "text": format!("message serialization failed: {e}"),
            })]
        }
    }
}
