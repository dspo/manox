//! `ServerNote → events.rs/session.rs JSON` for the WebUI TS store.
//!
//! The AgentServer emits typed [`ServerNote`] (manox-protocol); the WebUI
//! store consumes the legacy `events.rs`/`session.rs` JSON shapes (routed by
//! `bridge::on_event`). This is the δ₁ adapter that projects each `ServerNote`
//! back onto that legacy shape so the store is unchanged. Shapes mirror
//! `manox-session-core::events::thread_event_to_json` (ThreadEvent projections)
//! and `session.rs` direct emits (AgentServer-dispatched notes) field-for-field;
//! a few `ServerNote` variants carry a different encoding than the legacy store
//! (noted inline) and pass through best-effort — the spine (dispatch + pump +
//! routing) is the validation target, display polish is follow-up.

use manox_protocol::ServerNote;
use serde_json::{Value, json};

/// Project one `ServerNote` onto the legacy WebUI JSON shape. `None` means
/// the store does not render this note (consumed or unsupported).
#[allow(dead_code)] // consumed by the δ₁-b bridge rewire (pump.rs), not yet wired.
pub fn server_note_to_webview_json(note: &ServerNote) -> Option<Value> {
    Some(match note {
        ServerNote::Ready => return None,
        // AgentServer-dispatched (mirror session.rs direct emits).
        ServerNote::SessionCreated { session_id } => {
            json!({"type": "session_created", "sessionId": session_id})
        }
        ServerNote::SessionDisposed { session_id } => {
            json!({"type": "session_disposed", "sessionId": session_id})
        }
        ServerNote::ThreadHistory {
            session_id,
            messages,
            display_history,
            auto_approved_tools,
            restored,
            loading,
        } => json!({
            // Legacy store reads `messages` + `auto_approved_tools` (snake);
            // the typed protocol extras (displayHistory/restored/loading) are
            // shipped snake-cased so a future typed store can read them, while
            // the legacy store ignores them.
            "type": "thread_history",
            "sessionId": session_id,
            "messages": messages,
            "display_history": display_history,
            "auto_approved_tools": auto_approved_tools,
            "restored": restored,
            "loading": loading,
        }),
        ServerNote::ThreadInfo { session_id, info } => json!({
            // The legacy store's `mergeInfo` reads snake_case keys
            // (reasoning_effort/usage/cost/…) off `info`. ThreadInfoPayload
            // (typed β-1) carries the metadata but NOT usage/cost/agents —
            // those live on ServerNote::UsageSnapshot. Ship the legacy shape
            // with empty defaults for the missing aggregates so the store
            // renders blanks instead of crashing; a typed store (approach b,
            // δ₁-b/γ) reads UsageSnapshot for the real numbers.
            "type": "thread_info",
            "sessionId": session_id,
            "info": {
                "reasoning_effort": info.reasoning_effort,
                "worktree_path": info.worktree_path,
                "plan": null,
                "goal": info.goal,
                "usage": {},
                "per_model_usage": {},
                "per_model_last_usage": {},
                "per_model_cost": {},
                "cost": 0,
                "pending_auth_count": 0,
                "agents": [],
            },
        }),
        ServerNote::ThreadsUpdated { threads } => {
            json!({"type": "threads_updated", "threads": threads})
        }
        ServerNote::Models { models } => json!({"type": "models", "models": models}),
        ServerNote::Usage {
            session_id,
            usage,
            cost,
        } => json!({"type": "usage", "sessionId": session_id, "usage": usage, "cost": cost}),
        // Legacy `model_changed` carries `from`+`to`; ServerNote::CurrentModel has
        // only id/name — `from` is dropped in translate (best-effort).
        ServerNote::CurrentModel {
            session_id,
            id,
            name,
        } => json!({
            "type": "current_model",
            "sessionId": session_id,
            "id": id,
            "name": name,
        }),
        ServerNote::SteerPending {
            session_id,
            client_id,
            message_id,
        } => json!({
            "type": "steer_pending",
            "sessionId": session_id,
            "clientId": client_id,
            "messageId": message_id,
        }),
        ServerNote::PermissionModeChanged { session_id, mode } => {
            json!({"type": "approval_mode_changed", "sessionId": session_id, "mode": mode})
        }
        ServerNote::ReasoningEffortChanged { session_id, effort } => json!({
            "type": "reasoning_effort_changed",
            "sessionId": session_id,
            "effort": effort,
        }),
        ServerNote::BrowserSuitesChanged { session_id, suites } => json!({
            "type": "browser_suites_changed",
            "sessionId": session_id,
            "suites": suites,
        }),
        ServerNote::CompactionStarted {
            session_id,
            tokens_before,
        } => json!({
            "type": "compaction_started",
            "sessionId": session_id,
            "tokensBefore": tokens_before,
        }),
        // ThreadEvent projections (mirror events.rs field-for-field).
        ServerNote::TurnStarted { session_id } => {
            json!({"type": "turn_started", "sessionId": session_id})
        }
        ServerNote::TurnFinished {
            session_id,
            cancelled,
            failed,
            stranded_steer_ids,
        } => json!({
            "type": "turn_finished",
            "sessionId": session_id,
            "cancelled": cancelled,
            "failed": failed,
            "strandedSteerIds": stranded_steer_ids,
        }),
        ServerNote::Stop { session_id, reason } => {
            json!({"type": "stop", "sessionId": session_id, "reason": reason})
        }
        ServerNote::AgentText { session_id, text } => {
            json!({"type": "agent_text", "sessionId": session_id, "text": text})
        }
        ServerNote::AgentThinking { session_id, text } => {
            json!({"type": "agent_thinking", "sessionId": session_id, "text": text})
        }
        ServerNote::ToolCall {
            session_id,
            id,
            name,
            title,
            status,
            input,
        } => json!({
            "type": "tool_call",
            "sessionId": session_id,
            "id": id,
            "name": name,
            "title": title,
            "status": status,
            "input": input,
        }),
        ServerNote::ToolResult {
            session_id,
            id,
            output,
            is_error,
        } => json!({
            "type": "tool_result",
            "sessionId": session_id,
            "id": id,
            "output": output,
            "is_error": is_error,
        }),
        ServerNote::ToolOutput {
            session_id,
            id,
            chunk,
        } => json!({
            "type": "tool_output",
            "sessionId": session_id,
            "id": id,
            "chunk": chunk,
        }),
        ServerNote::SteerInjected {
            session_id,
            message_id,
        } => json!({
            "type": "steer_injected",
            "sessionId": session_id,
            "messageId": message_id,
        }),
        ServerNote::TokenUsage {
            session_id,
            input,
            output,
            cache_creation,
            cache_read,
        } => json!({
            "type": "token_usage",
            "sessionId": session_id,
            "input": input,
            "output": output,
            "cache_creation": cache_creation,
            "cache_read": cache_read,
        }),
        ServerNote::SubagentStarted {
            session_id,
            id,
            agent_type,
            description,
        } => json!({
            "type": "subagent_started",
            "sessionId": session_id,
            "id": id,
            "agent_type": agent_type,
            "description": description,
        }),
        ServerNote::SubagentProgress {
            session_id,
            id,
            agent_type,
            tool_uses,
            latest_activity,
            status,
        } => json!({
            "type": "subagent_progress",
            "sessionId": session_id,
            "id": id,
            "agent_type": agent_type,
            "tool_uses": tool_uses,
            "latest_activity": latest_activity,
            "status": status,
            // ServerNote lacks `health` (events.rs carries it); the store reads
            // it optionally — leave absent rather than invent a value.
        }),
        // ServerNote::SubagentChild.event is translate's `{"debug":..}`; events.rs
        // projects the real child shape — passed through best-effort.
        ServerNote::SubagentChild {
            session_id,
            id,
            event,
        } => json!({
            "type": "subagent_child",
            "sessionId": session_id,
            "id": id,
            "event": event,
        }),
        ServerNote::WorktreeChanged {
            session_id,
            active,
            path,
        } => json!({
            "type": "worktree_changed",
            "sessionId": session_id,
            "active": active,
            "path": path,
        }),
        ServerNote::PlanReady {
            session_id,
            plan_file,
            title,
            content,
        } => json!({
            "type": "plan_ready",
            "sessionId": session_id,
            "plan_file": plan_file,
            "title": title,
            "content": content,
        }),
        ServerNote::PlanUpdated {
            session_id,
            snapshot,
        } => json!({
            "type": "plan_updated",
            "sessionId": session_id,
            "snapshot": snapshot,
        }),
        ServerNote::PlanModeChanged {
            session_id,
            enabled,
        } => json!({
            "type": "plan_mode_changed",
            "sessionId": session_id,
            "enabled": enabled,
        }),
        ServerNote::GoalChanged {
            session_id,
            snapshot,
        } => json!({
            "type": "goal_changed",
            "sessionId": session_id,
            "snapshot": snapshot,
        }),
        ServerNote::HistoryProgress { session_id } => {
            json!({"type": "history_progress", "sessionId": session_id})
        }
        // ServerNote::Compaction.summary is translate's formatted string; events.rs
        // uses the raw summary — passed through best-effort.
        ServerNote::Compaction {
            session_id,
            summary,
        } => json!({"type": "compaction", "sessionId": session_id, "summary": summary}),
        ServerNote::BackgroundTaskUpdated {
            session_id,
            snapshot,
        } => json!({
            "type": "background_task_updated",
            "sessionId": session_id,
            "snapshot": snapshot,
        }),
        ServerNote::Retry {
            session_id,
            attempt,
            max_attempts,
            delay_secs,
            reason,
            detail,
        } => json!({
            "type": "retry",
            "sessionId": session_id,
            "attempt": attempt,
            "max_attempts": max_attempts,
            "delay_secs": delay_secs,
            "reason": reason,
            "detail": detail,
        }),
        ServerNote::PeerMessage {
            session_id,
            from,
            content,
        } => json!({
            "type": "peer_message",
            "sessionId": session_id,
            "from": from,
            "content": content,
        }),
        ServerNote::Branch { session_id, branch } => {
            json!({"type": "branch", "sessionId": session_id, "branch": branch})
        }
        ServerNote::GitStats { session_id, stats } => {
            json!({"type": "git_stats", "sessionId": session_id, "stats": stats})
        }
        ServerNote::ApprovalDecision {
            session_id,
            tool_call_id,
            tool_name,
            tool_title,
            verdict,
            reason,
        } => json!({
            "type": "approval_decision",
            "sessionId": session_id,
            "tool_call_id": tool_call_id,
            "tool_name": tool_name,
            "tool_title": tool_title,
            "verdict": verdict,
            "reason": reason,
        }),
        // Bare-model completion (no session scope).
        ServerNote::ModelText { request_id, text } => {
            json!({"type": "model_text", "requestId": request_id, "text": text})
        }
        ServerNote::ModelThinking { request_id, text } => {
            json!({"type": "model_thinking", "requestId": request_id, "text": text})
        }
        ServerNote::ModelToolCall {
            request_id,
            id,
            name,
            input,
        } => json!({
            "type": "model_tool_call",
            "requestId": request_id,
            "id": id,
            "name": name,
            "input": input,
        }),
        ServerNote::ModelChatDone {
            request_id,
            stop,
            error,
        } => json!({
            "type": "model_chat_done",
            "requestId": request_id,
            "stop": stop,
            "error": error,
        }),
        ServerNote::Error {
            session_id,
            message,
        } => match session_id {
            Some(sid) => json!({"type": "error", "sessionId": sid, "message": message}),
            None => json!({"type": "error", "message": message}),
        },
        // UsageSnapshot has no legacy store shape yet; γ wires the typed client
        // store. Drop until then.
        ServerNote::UsageSnapshot { .. } => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ready_is_consumed() {
        assert!(server_note_to_webview_json(&ServerNote::Ready).is_none());
    }

    #[test]
    fn session_created_carries_session_id() {
        let v = server_note_to_webview_json(&ServerNote::SessionCreated {
            session_id: "s1".into(),
        })
        .unwrap();
        assert_eq!(v["type"], "session_created");
        assert_eq!(v["sessionId"], "s1");
    }

    #[test]
    fn turn_started_shape() {
        let v = server_note_to_webview_json(&ServerNote::TurnStarted {
            session_id: "s1".into(),
        })
        .unwrap();
        assert_eq!(v["type"], "turn_started");
        assert_eq!(v["sessionId"], "s1");
    }

    #[test]
    fn turn_finished_stranded_camel() {
        let v = server_note_to_webview_json(&ServerNote::TurnFinished {
            session_id: "s1".into(),
            cancelled: false,
            failed: false,
            stranded_steer_ids: vec!["m1".into()],
        })
        .unwrap();
        assert_eq!(v["strandedSteerIds"], serde_json::json!(["m1"]));
    }

    #[test]
    fn agent_text_shape() {
        let v = server_note_to_webview_json(&ServerNote::AgentText {
            session_id: "s1".into(),
            text: "hi".into(),
        })
        .unwrap();
        assert_eq!(v["type"], "agent_text");
        assert_eq!(v["text"], "hi");
        assert_eq!(v["sessionId"], "s1");
    }

    #[test]
    fn tool_call_passes_status_through() {
        let v = server_note_to_webview_json(&ServerNote::ToolCall {
            session_id: "s1".into(),
            id: "t1".into(),
            name: "Bash".into(),
            title: "run ls".into(),
            status: "running".into(),
            input: Some(serde_json::json!({"cmd": "ls"})),
        })
        .unwrap();
        assert_eq!(v["status"], "running");
        assert_eq!(v["input"]["cmd"], "ls");
    }

    #[test]
    fn thread_info_serializes_payload() {
        let payload = manox_protocol::server::ThreadInfoPayload {
            cwd: "/".into(),
            project: None,
            display_title: "T".into(),
            model_id: None,
            model_name: None,
            permission_mode: "workspace-write".into(),
            reasoning_effort: "high".into(),
            pinned: false,
            archived: false,
            depth: 0,
            agent_label: "lead".into(),
            self_author: "lead".into(),
            worktree_active: false,
            worktree_path: None,
            branch: None,
            goal: None,
            goal_elapsed_seconds: None,
            plan_mode: false,
            browser_suites: vec![],
            history_phase: "ready".into(),
            running: false,
            has_interacted: false,
        };
        let v = server_note_to_webview_json(&ServerNote::ThreadInfo {
            session_id: "s1".into(),
            info: Box::new(payload),
        })
        .unwrap();
        assert_eq!(v["type"], "thread_info");
        assert_eq!(v["info"]["reasoning_effort"], "high");
    }

    #[test]
    fn global_error_has_no_session_id() {
        let v = server_note_to_webview_json(&ServerNote::Error {
            session_id: None,
            message: "boom".into(),
        })
        .unwrap();
        assert_eq!(v["type"], "error");
        assert!(v.get("sessionId").is_none() || v["sessionId"].is_null());
        assert_eq!(v["message"], "boom");
    }

    #[test]
    fn model_text_is_request_scoped() {
        let v = server_note_to_webview_json(&ServerNote::ModelText {
            request_id: "r1".into(),
            text: "delta".into(),
        })
        .unwrap();
        assert_eq!(v["type"], "model_text");
        assert_eq!(v["requestId"], "r1");
        assert!(v.get("sessionId").is_none());
    }

    #[test]
    fn usage_snapshot_dropped() {
        assert!(
            server_note_to_webview_json(&ServerNote::UsageSnapshot {
                session_id: "s1".into(),
                cumulative: manox_protocol::server::TokenUsageSnapshot {
                    input: 0,
                    output: 0,
                    cache_creation: 0,
                    cache_read: 0,
                },
                per_model: Default::default(),
                cumulative_cost: 0.0,
                per_model_cost: Default::default(),
            })
            .is_none()
        );
    }
}
