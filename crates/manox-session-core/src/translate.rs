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
pub fn translate(ev: &agent::thread::ThreadEvent, session_id: &str) -> Translated {
    use agent::thread::ThreadEvent;

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
        } => Note(ServerNote::Compaction {
            session_id: session_id.into(),
            summary: format!("{summary} ({messages_compacted} msgs, {tokens_before} tokens)"),
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
            if tool_name == agent::tools::ASK_USER_QUESTION {
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
pub fn token_usage_snapshot(usage: &agent::language_model::TokenUsage) -> TokenUsageSnapshot {
    TokenUsageSnapshot {
        input: usage.input_tokens,
        output: usage.output_tokens,
        cache_creation: usage.cache_creation_input_tokens,
        cache_read: usage.cache_read_input_tokens,
    }
}

use Translated::*;
