//! Translation from kernel `ThreadEvent` to protocol `ServerNote` / `ServerCall`.
//!
//! The AgentServer's event pump receives `Arc<ThreadEvent>` from each
//! [`ThreadHandle::subscribe`] and routes them through this module:
//! streaming events become [`ServerNote`] (fire-and-forget to the client),
//! adjudication events become [`ServerCall`] (the server awaits the client's
//! [`FromClient::Reply`]), and diagnostic-only events are dropped (they carry
//! no UI-visible state and would clutter the wire for no projection benefit).

use manox_protocol::server::TokenUsageSnapshot;
use manox_protocol::{ServerCall, ServerNote};

/// The translation result for one `ThreadEvent`.
pub enum Translated {
    /// A streaming notification — fire-and-forget to the owning client(s).
    Note(ServerNote),
    /// An adjudication / capability call — the server issues it and awaits the
    /// client's [`FromClient::Reply`].
    Call(ServerCall),
    /// Not over the wire: diagnostic-only or handled by a different path.
    Skip,
}

/// Translate one kernel event into its protocol form.
pub fn translate(ev: &manox_agent::thread::ThreadEvent, session_id: &str) -> Translated {
    use manox_agent::thread::ThreadEvent;

    match ev {
        ThreadEvent::AgentText(text) => Note(ServerNote::AgentText {
            session_id: session_id.into(),
            text: text.clone(),
        }),
        ThreadEvent::AgentThinking(text) => Note(ServerNote::AgentThinking {
            session_id: session_id.into(),
            text: text.clone(),
        }),
        ThreadEvent::ToolCall {
            id,
            name,
            title,
            status,
            input,
        } => Note(ServerNote::ToolCall {
            session_id: session_id.into(),
            id: id.clone(),
            name: name.clone(),
            title: title.clone(),
            status: serde_json::to_value(status)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default(),
            input: input.clone(),
        }),
        ThreadEvent::ToolResult {
            id,
            output,
            is_error,
        } => Note(ServerNote::ToolResult {
            session_id: session_id.into(),
            id: id.clone(),
            output: output.clone(),
            is_error: *is_error,
        }),
        ThreadEvent::ToolOutput { id, chunk } => Note(ServerNote::ToolOutput {
            session_id: session_id.into(),
            id: id.clone(),
            chunk: chunk.clone(),
        }),
        ThreadEvent::TurnStarted => Note(ServerNote::TurnStarted {
            session_id: session_id.into(),
        }),
        ThreadEvent::Stop(reason) => Note(ServerNote::Stop {
            session_id: session_id.into(),
            reason: serde_json::to_value(reason)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string)),
        }),
        ThreadEvent::TurnFinished {
            cancelled,
            failed,
            stranded_steer_ids,
        } => Note(ServerNote::TurnFinished {
            session_id: session_id.into(),
            cancelled: *cancelled,
            failed: *failed,
            stranded_steer_ids: stranded_steer_ids.clone(),
        }),
        ThreadEvent::Retry {
            attempt,
            max_attempts,
            delay_secs,
            reason,
            detail,
        } => Note(ServerNote::Retry {
            session_id: session_id.into(),
            attempt: *attempt,
            max_attempts: *max_attempts,
            delay_secs: *delay_secs,
            reason: reason.clone(),
            detail: detail.clone(),
        }),
        ThreadEvent::Error(err) => Note(ServerNote::Error {
            session_id: Some(session_id.into()),
            message: format!("{err:#}"),
        }),
        ThreadEvent::ModelChanged { to, .. } => Note(ServerNote::CurrentModel {
            session_id: session_id.into(),
            id: None,
            name: Some(to.clone()),
        }),
        ThreadEvent::TokenUsageUpdated(usage) => Note(ServerNote::TokenUsage {
            session_id: session_id.into(),
            input: usage.input_tokens,
            output: usage.output_tokens,
            cache_creation: usage.cache_creation_input_tokens,
            cache_read: usage.cache_read_input_tokens,
        }),
        ThreadEvent::PermissionModeChanged { mode } => Note(ServerNote::PermissionModeChanged {
            session_id: session_id.into(),
            mode: serde_json::to_value(mode)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default(),
        }),
        ThreadEvent::ReasoningEffortChanged { effort } => {
            Note(ServerNote::ReasoningEffortChanged {
                session_id: session_id.into(),
                effort: format!("{effort:?}").to_lowercase(),
            })
        }
        ThreadEvent::BrowserSuitesChanged { suites } => Note(ServerNote::BrowserSuitesChanged {
            session_id: session_id.into(),
            suites: suites
                .iter()
                .map(|s| format!("{s:?}").to_lowercase())
                .collect(),
        }),
        ThreadEvent::PlanReady { plan_file, title } => Note(ServerNote::PlanReady {
            session_id: session_id.into(),
            plan_file: plan_file.clone(),
            title: title.clone(),
            content: None,
        }),
        ThreadEvent::PlanUpdated { snapshot } => Note(ServerNote::PlanUpdated {
            session_id: session_id.into(),
            snapshot: serde_json::to_value(snapshot).ok(),
        }),
        ThreadEvent::PlanModeChanged { enabled } => Note(ServerNote::PlanModeChanged {
            session_id: session_id.into(),
            enabled: *enabled,
        }),
        ThreadEvent::GoalChanged { goal } => Note(ServerNote::GoalChanged {
            session_id: session_id.into(),
            snapshot: serde_json::to_value(goal).ok(),
        }),
        ThreadEvent::CwdChanged { path } => Note(ServerNote::CwdChanged {
            session_id: session_id.into(),
            path: path.clone(),
        }),
        ThreadEvent::CompactionStarted { tokens_before } => Note(ServerNote::CompactionStarted {
            session_id: session_id.into(),
            tokens_before: *tokens_before,
        }),
        ThreadEvent::Compaction {
            summary,
            messages_compacted,
            tokens_before,
            retained_tail,
        } => Note(ServerNote::Compaction {
            session_id: session_id.into(),
            summary: format!("{summary} ({messages_compacted} msgs, {tokens_before} tokens)"),
            retained: serde_json::to_value(retained_tail)
                .unwrap_or(serde_json::Value::Array(Vec::new())),
        }),
        ThreadEvent::SubagentStarted {
            id,
            subagent_type,
            description,
            child: _,
        } => Note(ServerNote::SubagentStarted {
            session_id: session_id.into(),
            id: id.clone(),
            agent_type: subagent_type.clone(),
            description: description.clone(),
        }),
        ThreadEvent::SubagentProgress {
            id,
            subagent_type,
            tool_uses,
            token_usage: _,
            latest_activity,
            status,
            health: _,
        } => Note(ServerNote::SubagentProgress {
            session_id: session_id.into(),
            id: id.clone(),
            agent_type: subagent_type.clone(),
            tool_uses: *tool_uses,
            latest_activity: latest_activity.clone(),
            status: serde_json::to_value(status)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default(),
        }),
        ThreadEvent::SubagentChild { id, child } => Note(ServerNote::SubagentChild {
            session_id: session_id.into(),
            id: id.clone(),
            event: serde_json::to_value(child).unwrap_or(serde_json::Value::Null),
        }),
        ThreadEvent::BackgroundTaskUpdated { snapshot } => {
            Note(ServerNote::BackgroundTaskUpdated {
                session_id: session_id.into(),
                snapshot: serde_json::to_value(snapshot).unwrap_or(serde_json::Value::Null),
            })
        }
        ThreadEvent::SteerInjected { message_id } => Note(ServerNote::SteerInjected {
            session_id: session_id.into(),
            message_id: message_id.clone(),
        }),
        ThreadEvent::PeerMessage { from, content } => Note(ServerNote::PeerMessage {
            session_id: session_id.into(),
            from: from.clone(),
            content: content.clone(),
        }),
        ThreadEvent::HistoryProgress => Note(ServerNote::HistoryProgress {
            session_id: session_id.into(),
        }),
        ThreadEvent::HistoryRestored => Skip,
        // The persisted display title rides the `ThreadInfo` snapshot's
        // `display_title` (rebuilt from the facade on Ready/attach); the live
        // event carries no state the client has not already received.
        ThreadEvent::TitleChanged { .. } => Skip,
        ThreadEvent::ToolCallAuthorization {
            id,
            tool_name,
            summary,
            input,
        } => {
            // AskUserQuestion's authorization is an interactive question, not a
            // bare allow/deny: route it as its own ServerCall kind so the
            // client renders the ask card and returns structured answers.
            if tool_name == manox_agent::tools::ASK_USER_QUESTION {
                Call(ServerCall::AskUserQuestion {
                    session_id: session_id.into(),
                    auth_id: id.clone(),
                    input: input.clone(),
                })
            } else {
                Call(ServerCall::Approve {
                    session_id: session_id.into(),
                    auth_id: id.clone(),
                    tool_name: tool_name.clone(),
                    summary: summary.clone(),
                    input: input.clone(),
                })
            }
        }
        ThreadEvent::PrefixStability { .. }
        | ThreadEvent::SideCallMetricsUpdated(_)
        | ThreadEvent::MainCallMetricsUpdated(_) => Skip,
        ThreadEvent::CacheInvalidation { reprocessed_tokens } => {
            Note(ServerNote::CacheInvalidation {
                session_id: session_id.into(),
                reprocessed_tokens: *reprocessed_tokens,
            })
        }
    }
}

/// Build a `TokenUsageSnapshot` from a kernel `TokenUsage`.
pub fn token_usage_snapshot(usage: &manox_agent::language_model::TokenUsage) -> TokenUsageSnapshot {
    TokenUsageSnapshot {
        input: usage.input_tokens,
        output: usage.output_tokens,
        cache_creation: usage.cache_creation_input_tokens,
        cache_read: usage.cache_read_input_tokens,
    }
}

// ── v4 journal → wire mapping (§C.2 / §D.1, T4) ─────────────────────────────
//
// The kernel `SessionTreeEntry` vocabulary (37 variants, journal v4) projects
// onto the wire [`manox_protocol::JournalWireEvent`] (34 variants). Three
// kernel variants have no §C.2 wire row — `ActiveToolsChange`, `Custom`,
// `CustomMessage` — and map to `None` (the follow stream skips them without
// opening a §F.1 gap: an unclaimed seq is sealed by the next Entry). The
// two new wire fields carried by every entry (`id`/`parentId`) and the
// `seq` stamp ride [`wire_entry`]; `id → callId` / `agentId` renames follow
// the §C.1 envelope-key exclusivity rule.

use manox_harness::session::SessionTreeEntry;
use manox_harness::types::AgentMessage;
use manox_protocol::journal::{JournalWireEntry, JournalWireEvent, ModelRef, UsagePayload};

/// One journal record as the wire carries it (§C.1 entry envelope). `None`
/// when the kernel variant has no §C.2 wire row.
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

/// The §C.2 event projection of one kernel entry. `None` = no wire row
/// (see the module comment above).
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
            },
            AgentMessage::ToolResult { .. } | AgentMessage::BashExecution { .. } => W::Message {
                role: "tool".into(),
                content: message_value(message),
                usage: None,
                origin_rpc: None,
            },
            // Kernel-extension message roles ride the generic transcript row
            // with the kernel JSON shape verbatim (wire-opaque, §C.2).
            AgentMessage::Custom { .. } => W::Message {
                role: "custom".into(),
                content: message_value(message),
                usage: None,
                origin_rpc: None,
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
        SessionTreeEntry::PlanModeChange { enabled, .. } => W::PlanModeChange { enabled: *enabled },
        SessionTreeEntry::PlanUpdate { snapshot, .. } => W::PlanUpdate {
            snapshot: snapshot.clone(),
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
        SessionTreeEntry::ActiveToolsChange { .. }
        | SessionTreeEntry::Custom { .. }
        | SessionTreeEntry::CustomMessage { .. } => return None,
    })
}

/// Kernel content blocks pass through the wire opaque (§C.2 "storage shape");
/// an unspecifiable block is dropped rather than failing the frame.
fn content_blocks(blocks: &[manox_harness::types::ContentBlock]) -> Vec<serde_json::Value> {
    blocks
        .iter()
        .filter_map(|b| serde_json::to_value(b).ok())
        .collect()
}

/// The full kernel JSON object of a non-transcript message (wire-opaque).
fn message_value(message: &AgentMessage) -> Vec<serde_json::Value> {
    vec![serde_json::to_value(message).unwrap_or(serde_json::Value::Null)]
}

use Translated::*;

#[cfg(test)]
mod tests {
    use super::*;

    /// The compaction tail must flow through the REAL translate path, not a
    /// synthetic empty array: the client store replaces its transcript with
    /// `summary + retained`, so an empty-by-default field would wipe every
    /// protocol client's transcript on each real compaction.
    #[test]
    fn compaction_note_carries_the_retained_tail() {
        let tail = vec![
            manox_agent::message::Message::assistant(vec![
                manox_agent::language_model::MessageContent::Text("kept answer".into()),
            ]),
            manox_agent::message::Message::user("kept follow-up".into()),
        ];
        let translated = translate(
            &::manox_agent::thread::ThreadEvent::Compaction {
                summary: "folded".into(),
                messages_compacted: 9,
                tokens_before: 100_000,
                retained_tail: tail.clone(),
            },
            "s1",
        );
        let Translated::Note(ServerNote::Compaction { retained, .. }) = translated else {
            panic!("expected a Compaction note");
        };
        let round: Vec<manox_agent::message::Message> =
            serde_json::from_value(retained).expect("retained deserializes back");
        assert_eq!(round.len(), 2, "the tail survives the wire form");
        assert_eq!(round.len(), tail.len());
    }

    /// §J.4 (d) coverage: the kernel `SessionTreeEntry` vocabulary has 37
    /// variants; `wire_event` maps every one of them — the 34 §C.2 wire rows
    /// are produced, and the 3 kernel-only rows (`ActiveToolsChange`,
    /// `Custom`, `CustomMessage`) are the documented `None` drops. If a new
    /// kernel variant is added, the exhaustive `match` in `wire_event` stops
    /// compiling, so this test is the behavioral pin, not a completeness
    /// check — it asserts the drop-set is exactly the three named rows.
    #[test]
    fn wire_projection_covers_every_kernel_variant_and_drops_exactly_three() {
        use manox_harness::session::SessionTreeEntry as E;
        let now = chrono::Utc::now();
        let id = || "e".to_string();
        let pid = || Option::<String>::None;
        let all = vec![
            E::Message {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                message: AgentMessage::User {
                    content: vec![],
                    timestamp: now,
                },
                origin: None,
            },
            E::Compaction {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                summary: "s".into(),
                first_kept_entry_id: None,
                tokens_before: 0,
                retained_tail: None,
                usage: None,
                details: None,
                from_hook: None,
            },
            E::ModelChange {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                provider: "p".into(),
                model_id: "m".into(),
            },
            E::ThinkingLevelChange {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                thinking_level: "high".into(),
            },
            E::CwdChange {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                cwd: "/x".into(),
            },
            E::ActiveToolsChange {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                active_tool_names: vec![],
            },
            E::BranchSummary {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                from_id: "f".into(),
                summary: "s".into(),
                details: None,
                usage: None,
                from_hook: None,
            },
            E::Custom {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                custom_type: "c".into(),
                data: None,
            },
            E::CustomMessage {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                custom_type: "c".into(),
                content: vec![],
                details: None,
                display: false,
            },
            E::Label {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                target_id: "t".into(),
                label: Some("l".into()),
            },
            E::SessionInfo {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                name: Some("n".into()),
            },
            E::Leaf {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                target_id: Some("t".into()),
            },
            E::UiNote {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                note: serde_json::json!({"kind": "error"}),
            },
            E::TurnStart {
                id: id(),
                parent_id: pid(),
                timestamp: now,
            },
            E::TurnFinish {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                cancelled: false,
                failed: false,
                stranded_steer_ids: vec![],
            },
            E::Stop {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                reason: None,
            },
            E::Retry {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                attempt: 1,
                max_attempts: 2,
                delay_secs: 3,
                reason: "r".into(),
                detail: None,
            },
            E::ErrorEvent {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                message: "m".into(),
            },
            E::AgentTextDelta {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                delta: "d".into(),
            },
            E::AgentThinkingDelta {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                delta: "d".into(),
            },
            E::ToolCall {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                call_id: "c".into(),
                name: "n".into(),
                title: "t".into(),
                status: "running".into(),
                input: None,
            },
            E::ToolResult {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                call_id: "c".into(),
                output: "o".into(),
                is_error: false,
            },
            E::ToolOutputChunk {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                call_id: "c".into(),
                chunk: "x".into(),
            },
            E::SubagentChild {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                agent_id: "a".into(),
                event: serde_json::json!({}),
            },
            E::SubagentProgress {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                agent_id: "a".into(),
                agent_type: "t".into(),
                tool_uses: 0,
                latest_activity: None,
                status: "running".into(),
            },
            E::ProjectChange {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                path: None,
            },
            E::PermissionModeChange {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                mode: "read-only".into(),
            },
            E::PlanModeChange {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                enabled: true,
            },
            E::PlanUpdate {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                snapshot: serde_json::json!({}),
            },
            E::Goal {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                goal: None,
            },
            E::Title {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                title: "t".into(),
            },
            E::BrowserSuites {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                suites: vec![],
            },
            E::BackgroundTask {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                snapshot: serde_json::json!({}),
            },
            E::Approval {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                kind: "request".into(),
                auth_id: "a".into(),
                payload: serde_json::json!({}),
            },
            E::PinnedArchived {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                pinned: false,
                archived: false,
            },
            E::CompactionStarted {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                tokens_before: 0,
            },
            E::Metrics {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                metric_type: "side_call".into(),
                data: serde_json::json!({}),
            },
        ];
        assert_eq!(all.len(), 37, "kernel vocabulary is 37 variants");
        let dropped: Vec<&str> = all
            .iter()
            .filter(|e| wire_event(e).is_none())
            .map(|e| {
                // Kernel entry type tag (matches §C.1 `entry_kind`).
                manox_harness::session::entry_kind(e).as_str()
            })
            .collect();
        let mut dropped = dropped;
        dropped.sort_unstable();
        assert_eq!(
            dropped,
            vec!["active_tools_change", "custom", "custom_message"],
            "the drop-set is exactly the three kernel-only rows"
        );
        // The other 34 all project (and round-trip through the wire enum).
        for e in &all {
            if wire_event(e).is_none() {
                continue;
            }
            let entry = wire_entry(0, e).expect("mapped variant yields an entry");
            let json = serde_json::to_value(&entry).expect("entry serializes");
            let back: JournalWireEntry = serde_json::from_value(json).expect("entry parses");
            assert_eq!(
                entry,
                back,
                "wire round-trip for {}",
                manox_harness::session::entry_kind(e).as_str()
            );
        }
    }
}
