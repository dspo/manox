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

use manox_protocol::client::ImageAttachment;
use manox_protocol::{ClientCall, ClientNote, FromClient, MsgId, ServerCall, ServerNote};
use serde_json::{Value, json};

use crate::bridge::ReadyKind;

/// The `session_ready` metadata a create/open/plan_execute_fresh message
/// announces (id, kind, cwd), mirroring `bridge::translate`'s ready tuple so
/// the δ₁-b shuttle can pre-register `pending_ready` for `on_event`.
pub fn webview_ready_metadata(msg: &Value, cwd: &str) -> Option<(String, ReadyKind, String)> {
    match msg["type"].as_str()? {
        "new_session" => {
            let id = msg["sessionId"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            Some((id, ReadyKind::Fresh, cwd.to_string()))
        }
        "open_thread" => {
            let id = msg["sessionId"].as_str()?.to_string();
            Some((id, ReadyKind::Restored, cwd.to_string()))
        }
        "plan_execute_fresh" => {
            let fresh = uuid::Uuid::new_v4().to_string();
            let cwd = msg["cwd"].as_str().unwrap_or(cwd).to_string();
            Some((fresh, ReadyKind::Fresh, cwd))
        }
        _ => None,
    }
}

/// Project one `ServerNote` onto the legacy WebUI JSON shape. `None` means
/// the store does not render this note (consumed or unsupported).
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
        ServerNote::CacheInvalidation {
            session_id,
            reprocessed_tokens,
        } => json!({
            "type": "cache_invalidation",
            "sessionId": session_id,
            "reprocessedTokens": reprocessed_tokens,
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

/// Project a `ServerCall` (an adjudication/capability request the host must
/// surface to the user) onto the legacy browser-card JSON the store renders.
/// The WebUI answers via `webview_to_from_client` (`approve`/`plan_verdict`/
/// `answer_question`), correlating by the id the AgentServer used as MsgId.
pub fn server_call_to_webview_json(call: &ServerCall) -> Option<Value> {
    Some(match call {
        ServerCall::Approve {
            session_id,
            auth_id,
            tool_name,
            summary,
            input,
        } => json!({
            "type": "tool_call_authorization",
            "sessionId": session_id,
            "id": auth_id,
            "tool_name": tool_name,
            "summary": summary,
            "input": input,
        }),
        // AskUserQuestion shares the authorization-card surface; the store
        // keys the ask card off `tool_name == "AskUserQuestion"`.
        ServerCall::AskUserQuestion {
            session_id,
            auth_id,
            input,
        } => json!({
            "type": "tool_call_authorization",
            "sessionId": session_id,
            "id": auth_id,
            "tool_name": "AskUserQuestion",
            "summary": "",
            "input": input,
        }),
        ServerCall::PlanVerdict {
            session_id,
            plan_file,
            title,
            content,
        } => json!({
            "type": "plan_ready",
            "sessionId": session_id,
            "plan_file": plan_file,
            "title": title,
            "content": content.clone().unwrap_or_default(),
        }),
        // BrowserOp/ClipboardRead/OpenExternal: the WebUI declares no such
        // capability, so the AgentServer never routes these to it. If one
        // arrives, drop (fail-closed is the AgentServer's concern).
        ServerCall::BrowserOp { .. }
        | ServerCall::ClipboardRead { .. }
        | ServerCall::OpenExternal { .. } => return None,
    })
}

/// Translate one `WebviewToHost` message into the protocol `FromClient`
/// sequence it maps to. Pure (no bridge state): adjudication replies echo the
/// id the host already has — `approve`/`answer_question` use the auth_id,
/// `plan_verdict` the session id — matching the AgentServer's deterministic
/// `MsgId` per `ServerCall` kind, so no pending-id table is needed.
pub fn webview_to_from_client(
    msg: &Value,
    cwd: &str,
    id_override: Option<&str>,
) -> Vec<FromClient> {
    let ty = msg["type"].as_str().unwrap_or("");
    let sid = msg["sessionId"].as_str().map(str::to_string);
    fn img(msg: &Value) -> Vec<ImageAttachment> {
        msg.get("images")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(|i| {
                        let data = i.get("data")?.as_str().and_then(base64_decode)?;
                        Some(ImageAttachment {
                            data,
                            mime_type: i
                                .get("mimeType")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .into(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
    match ty {
        "submit" => vec![FromClient::Notification {
            note: ClientNote::Submit {
                session_id: sid.unwrap_or_default(),
                text: msg["text"].as_str().unwrap_or_default().to_string(),
                images: img(msg),
                client_id: msg["clientId"].as_str().map(str::to_string),
            },
        }],
        "steer" => vec![FromClient::Notification {
            note: ClientNote::Steer {
                session_id: sid.unwrap_or_default(),
                client_id: msg["clientId"].as_str().unwrap_or_default().to_string(),
                text: msg["text"].as_str().unwrap_or_default().to_string(),
                images: img(msg),
            },
        }],
        "drop_queued" => vec![FromClient::Notification {
            note: ClientNote::DropQueued {
                session_id: sid.unwrap_or_default(),
                client_id: msg["clientId"].as_str().unwrap_or_default().to_string(),
            },
        }],
        // The client answers a ServerCall::Approve; the id is the auth_id the
        // AgentServer used as the MsgId.
        "approve" => vec![FromClient::Reply {
            id: MsgId::new(msg["id"].as_str().unwrap_or_default()),
            outcome: Ok(json!({"allow": msg["allow"]})),
        }],
        "answer_question" => vec![FromClient::Reply {
            id: MsgId::new(msg["id"].as_str().unwrap_or_default()),
            outcome: Ok(json!({"answers": msg["answers"], "response": msg["response"]})),
        }],
        // plan_verdict has no id; the AgentServer used the session id as the MsgId.
        "plan_verdict" => vec![FromClient::Reply {
            id: MsgId::new(sid.clone().unwrap_or_default()),
            outcome: Ok(json!({"choice": msg["choice"]})),
        }],
        "cancel" => vec![FromClient::Notification {
            note: ClientNote::CancelTurn {
                session_id: sid.unwrap_or_default(),
            },
        }],
        "set_model" => vec![FromClient::Notification {
            note: ClientNote::SetModel {
                session_id: sid.unwrap_or_default(),
                id: msg["id"].as_str().unwrap_or_default().to_string(),
            },
        }],
        "set_reasoning_effort" => vec![FromClient::Notification {
            note: ClientNote::SetReasoningEffort {
                session_id: sid.unwrap_or_default(),
                effort: msg["effort"].as_str().unwrap_or_default().to_string(),
            },
        }],
        "set_approval_mode" => vec![FromClient::Notification {
            note: ClientNote::SetApprovalMode {
                session_id: sid.unwrap_or_default(),
                mode: msg["mode"].as_str().unwrap_or_default().to_string(),
            },
        }],
        "set_plan_mode" => vec![FromClient::Notification {
            note: ClientNote::SetPlanMode {
                session_id: sid.unwrap_or_default(),
                enabled: msg["enabled"].as_bool().unwrap_or(false),
            },
        }],
        "goal" => vec![FromClient::Notification {
            note: ClientNote::Goal {
                session_id: sid.unwrap_or_default(),
                action: msg["action"].as_str().unwrap_or_default().to_string(),
                objective: msg["objective"].as_str().map(str::to_string),
                budget: msg["budget"].as_u64(),
                max_rounds: None,
            },
        }],
        "stop_background_task" => vec![FromClient::Notification {
            note: ClientNote::StopBackgroundTask {
                task_id: msg["taskId"].as_str().unwrap_or_default().to_string(),
                session_id: sid.unwrap_or_default(),
            },
        }],
        "archive_thread" => vec![FromClient::Notification {
            note: ClientNote::ArchiveThread {
                session_id: sid.unwrap_or_default(),
                archived: msg["archived"].as_bool().unwrap_or(true),
            },
        }],
        "pin_thread" => vec![FromClient::Notification {
            note: ClientNote::PinThread {
                session_id: sid.unwrap_or_default(),
                pinned: msg["pinned"].as_bool().unwrap_or(true),
            },
        }],
        "focus_thread" => vec![FromClient::Notification {
            note: ClientNote::FocusThread { session_id: sid },
        }],
        "request_models" => vec![FromClient::Request {
            id: MsgId::new("list_models"),
            call: ClientCall::ListModels,
        }],
        "request_usage" => vec![FromClient::Request {
            id: MsgId::new("get_usage"),
            call: ClientCall::GetUsage {
                session_id: sid.unwrap_or_default(),
            },
        }],
        "request_thread_info" => vec![FromClient::Request {
            id: MsgId::new("thread_info"),
            call: ClientCall::ThreadInfo {
                session_id: sid.unwrap_or_default(),
            },
        }],
        "list_threads" => vec![FromClient::Request {
            id: MsgId::new("list_threads"),
            call: ClientCall::ListThreads,
        }],
        "list_commands" => vec![FromClient::Request {
            id: MsgId::new("list_commands"),
            call: ClientCall::ListCommands,
        }],
        "open_thread" => {
            let Some(id) = sid else { return vec![] };
            vec![
                FromClient::Request {
                    id: MsgId::new("open"),
                    call: ClientCall::OpenSession {
                        session_id: id.clone(),
                    },
                },
                FromClient::Request {
                    id: MsgId::new("cur_model"),
                    call: ClientCall::GetCurrentModel { session_id: id },
                },
            ]
        }
        "new_session" => {
            let id = sid
                .or(id_override.map(str::to_string))
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            let mut out = vec![FromClient::Notification {
                note: ClientNote::CreateSession {
                    session_id: id.clone(),
                    cwd: Some(cwd.to_string()),
                },
            }];
            if let Some(model) = msg["modelId"].as_str() {
                out.push(FromClient::Notification {
                    note: ClientNote::SetModel {
                        session_id: id.clone(),
                        id: model.to_string(),
                    },
                });
            }
            out.push(FromClient::Request {
                id: MsgId::new("cur_model"),
                call: ClientCall::GetCurrentModel {
                    session_id: id.clone(),
                },
            });
            if msg["text"].as_str().is_some()
                || msg
                    .get("images")
                    .is_some_and(|a| a.as_array().is_some_and(|a| !a.is_empty()))
            {
                out.push(FromClient::Notification {
                    note: ClientNote::Submit {
                        session_id: id,
                        text: msg["text"].as_str().unwrap_or_default().to_string(),
                        images: img(msg),
                        client_id: None,
                    },
                });
            }
            out
        }
        "plan_execute_fresh" => {
            let Some(plan_file) = msg["planFile"].as_str() else {
                return vec![];
            };
            let Some(cwd) = msg["cwd"]
                .as_str()
                .map(str::to_string)
                .or(Some(cwd.to_string()))
            else {
                return vec![];
            };
            let fresh = id_override
                .map(str::to_string)
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            let mut out = Vec::new();
            if let Some(old) = sid {
                out.push(FromClient::Notification {
                    note: ClientNote::ArchiveThread {
                        session_id: old,
                        archived: true,
                    },
                });
            }
            out.push(FromClient::Notification {
                note: ClientNote::CreateSession {
                    session_id: fresh.clone(),
                    cwd: Some(cwd),
                },
            });
            out.push(FromClient::Notification {
                note: ClientNote::PlanSeedExecution {
                    session_id: fresh,
                    plan_file: plan_file.to_string(),
                },
            });
            out
        }
        _ => vec![],
    }
}

/// Decode a base64 data-URL/standalone string into bytes (the WebUI ships
/// image bytes base64-encoded; the protocol `ImageAttachment.data` is raw).
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.decode(s).ok()
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
            model: None,
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
                per_request: Default::default(),
            })
            .is_none()
        );
    }
    #[test]
    fn forward_submit_maps_to_client_note() {
        let msg = json!({"type":"submit","sessionId":"s1","text":"hi","clientId":"c1"});
        let out = webview_to_from_client(&msg, "/", None);
        assert_eq!(out.len(), 1);
        assert!(matches!(
            out[0],
            FromClient::Notification { note: ClientNote::Submit { ref session_id, .. } } if session_id == "s1"
        ));
    }

    #[test]
    fn forward_approve_is_reply_with_auth_id() {
        let msg = json!({"type":"approve","sessionId":"s1","id":"a1","allow":true});
        let out = webview_to_from_client(&msg, "/", None);
        assert_eq!(out.len(), 1);
        match &out[0] {
            FromClient::Reply { id, outcome } => {
                assert_eq!(id.0, "a1");
                assert!(outcome.is_ok());
            }
            _ => panic!("expected Reply"),
        }
    }

    #[test]
    fn forward_plan_verdict_uses_session_id_as_msgid() {
        let msg = json!({"type":"plan_verdict","sessionId":"pg","choice":"execute_keep"});
        let out = webview_to_from_client(&msg, "/", None);
        match &out[0] {
            FromClient::Reply { id, .. } => assert_eq!(id.0, "pg"),
            _ => panic!("expected Reply"),
        }
    }

    #[test]
    fn forward_new_session_orchestrates() {
        let msg = json!({"type":"new_session","sessionId":"n1","modelId":"m1","text":"go"});
        let out = webview_to_from_client(&msg, "/", None);
        assert!(out.len() >= 3); // CreateSession + SetModel + GetCurrentModel + Submit
        assert!(matches!(
            out[0],
            FromClient::Notification {
                note: ClientNote::CreateSession { .. }
            }
        ));
    }
    #[test]
    fn forward_maps_each_message_type() {
        // Every WebviewToHost type must produce at least one FromClient.
        let cwd = "/proj";
        let cases: &[(&str, Value)] = &[
            (
                "submit",
                json!({"type":"submit","sessionId":"s1","text":"hi"}),
            ),
            (
                "steer",
                json!({"type":"steer","sessionId":"s1","clientId":"c1","text":"hi"}),
            ),
            (
                "drop_queued",
                json!({"type":"drop_queued","sessionId":"s1","clientId":"c1"}),
            ),
            (
                "approve",
                json!({"type":"approve","sessionId":"s1","id":"a1","allow":true}),
            ),
            (
                "answer_question",
                json!({"type":"answer_question","sessionId":"s1","id":"q1","answers":[["a","b"]],"response":"r"}),
            ),
            (
                "plan_verdict",
                json!({"type":"plan_verdict","sessionId":"s1","choice":"execute_keep"}),
            ),
            ("cancel", json!({"type":"cancel","sessionId":"s1"})),
            (
                "set_model",
                json!({"type":"set_model","sessionId":"s1","id":"m1"}),
            ),
            (
                "set_reasoning_effort",
                json!({"type":"set_reasoning_effort","sessionId":"s1","effort":"high"}),
            ),
            (
                "set_approval_mode",
                json!({"type":"set_approval_mode","sessionId":"s1","mode":"danger-full-access"}),
            ),
            (
                "set_plan_mode",
                json!({"type":"set_plan_mode","sessionId":"s1","enabled":true}),
            ),
            (
                "goal",
                json!({"type":"goal","sessionId":"s1","action":"create","objective":"x"}),
            ),
            (
                "stop_background_task",
                json!({"type":"stop_background_task","sessionId":"s1","taskId":"t1"}),
            ),
            (
                "archive_thread",
                json!({"type":"archive_thread","sessionId":"s1","archived":true}),
            ),
            (
                "pin_thread",
                json!({"type":"pin_thread","sessionId":"s1","pinned":true}),
            ),
            (
                "focus_thread",
                json!({"type":"focus_thread","sessionId":"s1"}),
            ),
            ("request_models", json!({"type":"request_models"})),
            (
                "request_usage",
                json!({"type":"request_usage","sessionId":"s1"}),
            ),
            (
                "request_thread_info",
                json!({"type":"request_thread_info","sessionId":"s1"}),
            ),
            ("list_threads", json!({"type":"list_threads"})),
            ("list_commands", json!({"type":"list_commands"})),
            (
                "open_thread",
                json!({"type":"open_thread","sessionId":"s1"}),
            ),
            (
                "new_session",
                json!({"type":"new_session","sessionId":"n1"}),
            ),
            (
                "plan_execute_fresh",
                json!({"type":"plan_execute_fresh","sessionId":"old","planFile":"/p.md","cwd":"/proj"}),
            ),
        ];
        for (ty, msg) in cases {
            let out = webview_to_from_client(msg, cwd, None);
            assert!(
                !out.is_empty(),
                "WebviewToHost `{ty}` produced no FromClient"
            );
        }
    }
    #[test]
    fn server_call_approve_card_shape() {
        let call = ServerCall::Approve {
            session_id: "s1".into(),
            auth_id: "a1".into(),
            tool_name: "Bash".into(),
            summary: "run ls".into(),
            input: serde_json::json!({"cmd": "ls"}),
        };
        let v = server_call_to_webview_json(&call).unwrap();
        assert_eq!(v["type"], "tool_call_authorization");
        assert_eq!(v["id"], "a1");
        assert_eq!(v["tool_name"], "Bash");
        assert_eq!(v["input"]["cmd"], "ls");
    }

    #[test]
    fn server_call_ask_user_card_shape() {
        let call = ServerCall::AskUserQuestion {
            session_id: "s1".into(),
            auth_id: "q1".into(),
            input: serde_json::json!({"q": "color?"}),
        };
        let v = server_call_to_webview_json(&call).unwrap();
        assert_eq!(v["type"], "tool_call_authorization");
        assert_eq!(v["id"], "q1");
        assert_eq!(v["tool_name"], "AskUserQuestion");
    }

    #[test]
    fn server_call_plan_verdict_card_shape() {
        let call = ServerCall::PlanVerdict {
            session_id: "s1".into(),
            plan_file: "/p.md".into(),
            title: "T".into(),
            content: Some("# Plan".into()),
        };
        let v = server_call_to_webview_json(&call).unwrap();
        assert_eq!(v["type"], "plan_ready");
        assert_eq!(v["plan_file"], "/p.md");
        assert_eq!(v["content"], "# Plan");
        // Missing content falls back to "" (legacy shape), not null.
        let none = ServerCall::PlanVerdict {
            session_id: "s1".into(),
            plan_file: "/p.md".into(),
            title: "T".into(),
            content: None,
        };
        assert_eq!(server_call_to_webview_json(&none).unwrap()["content"], "");
    }
}
