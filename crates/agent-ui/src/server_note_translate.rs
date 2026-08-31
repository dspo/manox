//! Reverse translation: `ServerNote` / `ServerCall` → `ThreadEvent`.
//!
//! The desktop's conversation layer (`ConversationState::apply`) consumes
//! `ThreadEvent`, while the AgentServer spine delivers `FromServer`. This
//! module mirrors `manox-session-core::translate` (the forward direction) so
//! the pump can re-emit protocol messages as the events the workspace already
//! handles — zero change to the workspace's 601-line handler.
//!
//! Both directions are covered: `FromServer::Notification` maps via
//! [`server_note_to_thread_event`], `FromServer::Request` (adjudication
//! `ServerCall`s) via [`server_call_to_thread_event`].

use agent::ThreadEvent;
use agent::thread::{HistoryPhase, PermissionMode};
use manox_protocol::{ServerCall, ServerNote};

/// Project a `ServerNote` onto the `ThreadEvent` the desktop conversation
/// renders. `None` for notes with no `ThreadEvent` counterpart (session
/// lifecycle, snapshots, model-chat streams — the store mirrors those).
pub fn server_note_to_thread_event(note: &ServerNote) -> Option<ThreadEvent> {
    use ServerNote::*;
    Some(match note {
        AgentText { text, .. } => ThreadEvent::AgentText(text.clone()),
        AgentThinking { text, .. } => ThreadEvent::AgentThinking(text.clone()),
        ToolCall {
            id,
            name,
            title,
            status,
            input,
            ..
        } => ThreadEvent::ToolCall {
            id: id.clone(),
            name: name.clone(),
            title: title.clone(),
            status: parse_status(status),
            input: input.clone(),
        },
        ToolResult {
            id,
            output,
            is_error,
            ..
        } => ThreadEvent::ToolResult {
            id: id.clone(),
            output: output.clone(),
            is_error: *is_error,
        },
        ToolOutput { id, chunk, .. } => ThreadEvent::ToolOutput {
            id: id.clone(),
            chunk: chunk.clone(),
        },
        TurnStarted { .. } => ThreadEvent::TurnStarted,
        Stop { reason, .. } => ThreadEvent::Stop(
            serde_json::from_value(serde_json::Value::String(
                reason.clone().unwrap_or_default(),
            ))
            .unwrap_or(agent::language_model::StopReason::EndTurn),
        ),
        TurnFinished {
            cancelled,
            failed,
            stranded_steer_ids,
            ..
        } => ThreadEvent::TurnFinished {
            cancelled: *cancelled,
            failed: *failed,
            stranded_steer_ids: stranded_steer_ids.clone(),
        },
        Retry {
            attempt,
            max_attempts,
            delay_secs,
            reason,
            detail,
            ..
        } => ThreadEvent::Retry {
            attempt: *attempt,
            max_attempts: *max_attempts,
            delay_secs: *delay_secs,
            reason: reason.clone(),
            detail: detail.clone(),
        },
        Error { message, .. } => ThreadEvent::Error(anyhow::anyhow!("{}", message)),
        CurrentModel { name, .. } => ThreadEvent::ModelChanged {
            from: None,
            to: name.clone().unwrap_or_default(),
        },
        TokenUsage {
            input,
            output,
            cache_creation,
            cache_read,
            ..
        } => ThreadEvent::TokenUsageUpdated(agent::language_model::TokenUsage {
            input_tokens: *input,
            output_tokens: *output,
            cache_creation_input_tokens: *cache_creation,
            cache_read_input_tokens: *cache_read,
        }),
        PermissionModeChanged { mode, .. } => ThreadEvent::PermissionModeChanged {
            mode: parse_permission_mode(mode),
        },
        ReasoningEffortChanged { effort, .. } => ThreadEvent::ReasoningEffortChanged {
            effort: parse_reasoning_effort(effort),
        },
        BrowserSuitesChanged { suites, .. } => ThreadEvent::BrowserSuitesChanged {
            suites: suites
                .iter()
                .filter_map(|s| {
                    serde_json::from_value(serde_json::Value::String(s.clone()))
                        .ok()
                        .or_else(|| {
                            serde_json::from_value::<agent::pi_engine::BrowserSuite>(
                                serde_json::Value::String(s.clone()),
                            )
                            .ok()
                        })
                })
                .collect(),
        },
        PlanReady {
            plan_file, title, ..
        } => ThreadEvent::PlanReady {
            plan_file: plan_file.clone(),
            title: title.clone(),
        },
        PlanUpdated { snapshot, .. } => ThreadEvent::PlanUpdated {
            snapshot: serde_json::from_value(snapshot.clone()?).ok()?,
        },
        PlanModeChanged { enabled, .. } => ThreadEvent::PlanModeChanged { enabled: *enabled },
        GoalChanged { snapshot, .. } => ThreadEvent::GoalChanged {
            goal: snapshot
                .as_ref()
                .and_then(|s| serde_json::from_value(s.clone()).ok()),
        },
        CwdChanged { path, .. } => ThreadEvent::CwdChanged { path: path.clone() },
        CompactionStarted { tokens_before, .. } => ThreadEvent::CompactionStarted {
            tokens_before: *tokens_before,
        },
        Compaction { summary, .. } => ThreadEvent::Compaction {
            summary: summary.clone(),
            messages_compacted: 0,
            tokens_before: 0,
        },
        CacheInvalidation {
            reprocessed_tokens, ..
        } => ThreadEvent::CacheInvalidation {
            reprocessed_tokens: *reprocessed_tokens,
        },
        SubagentStarted {
            id,
            agent_type,
            description,
            ..
        } => ThreadEvent::SubagentStarted {
            id: id.clone(),
            subagent_type: agent_type.clone(),
            description: description.clone(),
            child: agent::ThreadId::default(),
        },
        SubagentProgress {
            id,
            agent_type,
            tool_uses,
            latest_activity,
            status,
            ..
        } => ThreadEvent::SubagentProgress {
            id: id.clone(),
            subagent_type: agent_type.clone(),
            tool_uses: *tool_uses,
            token_usage: agent::language_model::TokenUsage::default(),
            latest_activity: latest_activity.clone(),
            status: parse_status(status),
            health: None,
        },
        SubagentChild { id, event, .. } => ThreadEvent::SubagentChild {
            id: id.clone(),
            child: serde_json::from_value(event.clone()).ok()?,
        },
        BackgroundTaskUpdated { snapshot, .. } => ThreadEvent::BackgroundTaskUpdated {
            snapshot: serde_json::from_value(snapshot.clone()).ok()?,
        },
        SteerInjected { message_id, .. } => ThreadEvent::SteerInjected {
            message_id: message_id.clone(),
        },
        PeerMessage { from, content, .. } => ThreadEvent::PeerMessage {
            from: from.clone(),
            content: content.clone(),
        },
        HistoryProgress { .. } => ThreadEvent::HistoryProgress,
        // Every authoritative history boundary re-arms the conversation
        // rebuild; a mid-session restore (`restored: false`) and an
        // attach replay (`restored: true`) are both safe — the rebuild is
        // idempotent.
        ThreadHistory { .. } => ThreadEvent::HistoryRestored,
        // No ThreadEvent counterpart: session lifecycle, snapshots, model
        // chat, per-session streams the store mirrors instead.
        Ready
        | SessionCreated { .. }
        | SessionDisposed { .. }
        | ThreadInfo { .. }
        | ThreadsUpdated { .. }
        | Models { .. }
        | Usage { .. }
        | UsageSnapshot { .. }
        | SteerPending { .. }
        | ApprovalDecision { .. }
        | Branch { .. }
        | GitStats { .. }
        | ModelText { .. }
        | ModelThinking { .. }
        | ModelToolCall { .. }
        | ModelChatDone { .. } => return None,
    })
}

/// Project an adjudication `ServerCall` onto the `ThreadEvent` that renders
/// the approval / question card. The reply flows back through the pump's
/// pending-auth table (`FromClient::Reply`).
pub fn server_call_to_thread_event(call: &ServerCall) -> Option<ThreadEvent> {
    use ServerCall::*;
    match call {
        Approve {
            auth_id,
            tool_name,
            summary,
            input,
            ..
        } => Some(ThreadEvent::ToolCallAuthorization {
            id: auth_id.clone(),
            tool_name: tool_name.clone(),
            summary: summary.clone(),
            input: input.clone(),
        }),
        AskUserQuestion { auth_id, input, .. } => Some(ThreadEvent::ToolCallAuthorization {
            id: auth_id.clone(),
            tool_name: agent::tools::ASK_USER_QUESTION.to_string(),
            summary: String::new(),
            input: input.clone(),
        }),
        PlanVerdict {
            plan_file, title, ..
        } => Some(ThreadEvent::PlanReady {
            plan_file: plan_file.clone(),
            title: title.clone(),
        }),
        BrowserOp { .. } | ClipboardRead { .. } | OpenExternal { .. } => None,
    }
}

/// Reverse of `translate`'s ToolCallStatus projection (kebab-case string).
fn parse_status(status: &str) -> agent::thread::ToolCallStatus {
    serde_json::from_value(serde_json::Value::String(status.to_string()))
        .unwrap_or(agent::thread::ToolCallStatus::PendingApproval)
}

/// Reverse of `translate`'s PermissionMode projection (kebab-case string).
fn parse_permission_mode(mode: &str) -> PermissionMode {
    serde_json::from_value(serde_json::Value::String(mode.to_string()))
        .unwrap_or(PermissionMode::WorkspaceWrite)
}

/// Reverse of `translate`'s ReasoningEffort projection (snake-case string).
fn parse_reasoning_effort(effort: &str) -> agent::language_model::ReasoningEffort {
    serde_json::from_value(serde_json::Value::String(effort.to_string())).unwrap_or_default()
}

// Unused helper kept for the store mirror; `HistoryPhase` travels as a
// wire string through the store, never through `ThreadEvent`.
#[allow(dead_code)]
fn _history_phase(_: &str) -> HistoryPhase {
    HistoryPhase::Ready
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_text_maps() {
        let note = ServerNote::AgentText {
            session_id: "s1".into(),
            text: "hi".into(),
        };
        assert!(matches!(
            server_note_to_thread_event(&note),
            Some(ThreadEvent::AgentText(t)) if t == "hi"
        ));
    }

    #[test]
    fn tool_call_maps_status() {
        let note = ServerNote::ToolCall {
            session_id: "s1".into(),
            id: "tc-1".into(),
            name: "Bash".into(),
            title: "run".into(),
            status: "pending-approval".into(),
            input: Some(serde_json::json!({})),
        };
        assert!(matches!(
            server_note_to_thread_event(&note),
            Some(ThreadEvent::ToolCall { status, .. })
                if matches!(status, agent::thread::ToolCallStatus::PendingApproval)
        ));
    }

    #[test]
    fn error_maps_to_anyhow() {
        let note = ServerNote::Error {
            session_id: Some("s1".into()),
            message: "boom".into(),
        };
        assert!(matches!(
            server_note_to_thread_event(&note),
            Some(ThreadEvent::Error(_))
        ));
    }

    #[test]
    fn history_restored_derives_from_thread_history() {
        let note = ServerNote::ThreadHistory {
            session_id: "s1".into(),
            messages: serde_json::json!([]),
            display_history: serde_json::json!([]),
            auto_approved_tools: None,
            restored: false,
            loading: false,
        };
        assert!(matches!(
            server_note_to_thread_event(&note),
            Some(ThreadEvent::HistoryRestored)
        ));
    }

    #[test]
    fn cache_invalidation_maps() {
        let note = ServerNote::CacheInvalidation {
            session_id: "s1".into(),
            reprocessed_tokens: 42,
        };
        assert!(matches!(
            server_note_to_thread_event(&note),
            Some(ThreadEvent::CacheInvalidation {
                reprocessed_tokens: 42
            })
        ));
    }

    #[test]
    fn approve_call_maps_to_authorization() {
        let call = ServerCall::Approve {
            session_id: "s1".into(),
            auth_id: "auth-1".into(),
            tool_name: "Bash".into(),
            summary: "rm -rf".into(),
            input: serde_json::json!({}),
        };
        assert!(matches!(
            server_call_to_thread_event(&call),
            Some(ThreadEvent::ToolCallAuthorization { id, tool_name, .. })
                if id == "auth-1" && tool_name == "Bash"
        ));
    }

    #[test]
    fn ask_user_maps_to_authorization() {
        let call = ServerCall::AskUserQuestion {
            session_id: "s1".into(),
            auth_id: "auth-2".into(),
            input: serde_json::json!({}),
        };
        assert!(matches!(
            server_call_to_thread_event(&call),
            Some(ThreadEvent::ToolCallAuthorization { tool_name, .. })
                if tool_name == agent::tools::ASK_USER_QUESTION
        ));
    }

    #[test]
    fn plan_verdict_maps_to_plan_ready() {
        let call = ServerCall::PlanVerdict {
            session_id: "s1".into(),
            plan_file: "/plan.md".into(),
            title: "Plan".into(),
            content: None,
        };
        assert!(matches!(
            server_call_to_thread_event(&call),
            Some(ThreadEvent::PlanReady { plan_file, .. }) if plan_file == "/plan.md"
        ));
    }

    #[test]
    fn no_thread_event_for_snapshots() {
        let note = ServerNote::ThreadInfo {
            session_id: "s1".into(),
            info: Box::new(manox_protocol::server::ThreadInfoPayload {
                cwd: String::new(),
                project: None,
                display_title: String::new(),
                model_id: None,
                model_name: None,
                model: None,
                permission_mode: String::new(),
                reasoning_effort: String::new(),
                pinned: false,
                archived: false,
                depth: 0,
                agent_label: String::new(),
                self_author: String::new(),
                cwd_path: None,
                branch: None,
                goal: None,
                goal_elapsed_seconds: None,
                plan_mode: false,
                browser_suites: Vec::new(),
                history_phase: String::new(),
                running: false,
                has_interacted: false,
            }),
        };
        assert!(server_note_to_thread_event(&note).is_none());
    }

    #[test]
    fn browser_op_has_no_event() {
        let call = ServerCall::BrowserOp {
            session_id: "s1".into(),
            op: serde_json::json!({}),
        };
        assert!(server_call_to_thread_event(&call).is_none());
    }
}
