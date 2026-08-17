//! Mapping from `ThreadEvent` to JSON strings for the TypeScript host.
//!
//! `ThreadEvent` is not serde-serializable as a whole (it carries `Entity`,
//! `anyhow::Error`, and other non-`Serialize` payloads), so the host
//! renderers get an explicit projection. Unknown variants yield `None` and
//! are skipped by the caller.
//!
//! Variants intentionally not projected (silently dropped; surface them here
//! when the host grows the matching UI): `ApprovalDecision`, `Retry`,
//! `PrefixStability`, `CacheInvalidation`, `SideCallMetricsUpdated`,
//! `MainCallMetricsUpdated`, `CompactionStarted`, `PeerMessage`,
//! `SteerInjected`, `BrowserNotification`, `InboundAuthorization`,
//! `HistoryRestored`. `HistoryRestored` stays out of this pure projection:
//! the actor pairs it with a full `thread_history` snapshot that needs `App`
//! access to read the thread's messages. `GoalChanged` likewise pairs with a
//! rich `goal_changed` snapshot emitted by the actor's subscription.
//! `BackgroundTaskUpdated` and `SubagentChild` are projected here (the
//! mini-panel and task cards consume them).

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
        ThreadEvent::ReasoningEffortChanged { effort } => {
            json!({"type": "reasoning_effort_changed", "effort": effort.wire_value()})
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
        ThreadEvent::SubagentStarted {
            id,
            subagent_type,
            description,
            ..
        } => json!({
            "type": "subagent_started",
            "id": id,
            "agent_type": subagent_type,
            "description": description,
        }),
        ThreadEvent::SubagentProgress {
            id,
            subagent_type,
            tool_uses,
            latest_activity,
            status,
            ..
        } => json!({
            "type": "subagent_progress",
            "id": id,
            "agent_type": subagent_type,
            "tool_uses": tool_uses,
            "latest_activity": latest_activity,
            "status": status_str(status),
        }),
        ThreadEvent::WorktreeChanged { active, path } => {
            json!({"type": "worktree_changed", "active": active, "path": path})
        }
        // `PlanReady` is enriched (with the plan body) by the actor's
        // subscription; the pure projection would duplicate it bare.
        ThreadEvent::PlanReady { .. } => return None,
        ThreadEvent::PlanUpdated { snapshot } => json!({
            "type": "plan_updated",
            "snapshot": serde_json::to_value(snapshot).unwrap_or(Value::Null),
        }),
        ThreadEvent::PlanModeChanged { enabled } => {
            json!({"type": "plan_mode_changed", "enabled": enabled})
        }
        ThreadEvent::HistoryProgress => json!({"type": "history_progress"}),
        ThreadEvent::Compaction { summary, .. } => {
            json!({"type": "compaction", "summary": summary})
        }
        ThreadEvent::BackgroundTaskUpdated { snapshot } => json!({
            "type": "background_task_updated",
            "snapshot": serde_json::to_value(snapshot).unwrap_or(Value::Null),
        }),
        ThreadEvent::SubagentChild { id, child } => {
            let event = match child {
                agent::SubagentChildEvent::Text(text) => json!({"kind": "text", "text": text}),
                agent::SubagentChildEvent::Thinking(text) => {
                    json!({"kind": "thinking", "text": text})
                }
                agent::SubagentChildEvent::ToolStart { id, name, hint } => json!({
                    "kind": "tool_start",
                    "id": id,
                    "name": name,
                    "hint": hint
                        .as_ref()
                        .map(|(k, v)| json!({"key": k, "value": v})),
                }),
                agent::SubagentChildEvent::ToolEnd { id, name, is_error } => json!({
                    "kind": "tool_end",
                    "id": id,
                    "name": name,
                    "is_error": is_error,
                }),
            };
            json!({"type": "subagent_child", "id": id, "event": event})
        }
        _ => return None,
    };
    if let Some(id) = session_id
        && let Some(obj) = v.as_object_mut()
    {
        obj.insert("sessionId".to_string(), Value::String(id.to_string()));
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
    fn projects_reasoning_effort_changed() {
        let json = thread_event_to_json(
            &ThreadEvent::ReasoningEffortChanged {
                effort: agent::language_model::ReasoningEffort::Max,
            },
            Some("s1"),
        )
        .unwrap();
        let v: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "reasoning_effort_changed");
        assert_eq!(v["effort"], "max");
        assert_eq!(v["sessionId"], "s1");
    }

    #[test]
    fn drops_unprojected_variants() {
        // HistoryRestored is handled by the actor (it needs `App` access to
        // read messages), not by this pure projection.
        assert!(thread_event_to_json(&ThreadEvent::HistoryRestored, Some("s1")).is_none());
    }

    #[test]
    fn projects_plan_events() {
        // PlanReady is enriched (with the plan body) by the actor's
        // subscription; the pure projection drops it to avoid a duplicate.
        assert!(
            thread_event_to_json(
                &ThreadEvent::PlanReady {
                    plan_file: "/tmp/plan.md".into(),
                    title: "Do things".into(),
                },
                Some("s1"),
            )
            .is_none()
        );

        let snapshot = agent::plan::PlanSnapshot {
            explanation: Some("explanation".into()),
            steps: vec![agent::plan::PlanStep {
                step: "one".into(),
                status: agent::plan::PlanStepStatus::InProgress,
            }],
        };
        let json =
            thread_event_to_json(&ThreadEvent::PlanUpdated { snapshot }, Some("s1")).unwrap();
        let v: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "plan_updated");
        assert_eq!(v["snapshot"]["steps"][0]["step"], "one");

        let json =
            thread_event_to_json(&ThreadEvent::PlanModeChanged { enabled: true }, Some("s1"))
                .unwrap();
        let v: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "plan_mode_changed");
        assert_eq!(v["enabled"], true);
    }

    #[test]
    fn projects_worktree_changed() {
        let json = thread_event_to_json(
            &ThreadEvent::WorktreeChanged {
                active: true,
                path: Some("/repo/wt".into()),
            },
            Some("s1"),
        )
        .unwrap();
        let v: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "worktree_changed");
        assert_eq!(v["active"], true);
        assert_eq!(v["path"], "/repo/wt");
    }

    #[test]
    fn projects_subagent_progress() {
        let json = thread_event_to_json(
            &ThreadEvent::SubagentProgress {
                id: "sub1".into(),
                subagent_type: "explorer".into(),
                tool_uses: 3,
                token_usage: agent::TokenUsage::default(),
                latest_activity: Some("reading files".into()),
                status: agent::ToolCallStatus::Running,
            },
            Some("s1"),
        )
        .unwrap();
        let v: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "subagent_progress");
        assert_eq!(v["agent_type"], "explorer");
        assert_eq!(v["tool_uses"], 3);
        assert_eq!(v["status"], "running");
    }

    #[test]
    fn projects_history_progress() {
        let json = thread_event_to_json(&ThreadEvent::HistoryProgress, Some("s1")).unwrap();
        let v: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "history_progress");
        assert_eq!(v["sessionId"], "s1");
    }

    #[test]
    fn projects_compaction_summary() {
        let json = thread_event_to_json(
            &ThreadEvent::Compaction {
                summary: "older context".into(),
                messages_compacted: 12,
                tokens_before: 100_000,
            },
            Some("s1"),
        )
        .unwrap();
        let v: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "compaction");
        assert_eq!(v["summary"], "older context");
        assert_eq!(v["sessionId"], "s1");
    }

    #[test]
    fn projects_background_task_updated() {
        let snapshot = agent::background_task::TaskSnapshot {
            task_id: "mon_1".into(),
            kind: agent::background_task::TaskKind::MonitorCommand,
            owner_thread_id: "s1".into(),
            description: "watch build".into(),
            status: agent::background_task::TaskStatus::Completed,
            created_at_ms: 1_700_000_000_000,
            ended_at_ms: Some(1_700_000_001_000),
            event_count: 2,
            total_bytes: 42,
            exit_code: Some(0),
            failure_summary: None,
            anchor_message_id: None,
            output_tail: "hello\nworld".into(),
        };
        let json =
            thread_event_to_json(&ThreadEvent::BackgroundTaskUpdated { snapshot }, Some("s1"))
                .unwrap();
        let v: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "background_task_updated");
        assert_eq!(v["snapshot"]["task_id"], "mon_1");
        assert_eq!(v["snapshot"]["status"], "Completed");
        assert_eq!(v["snapshot"]["output_tail"], "hello\nworld");
        assert_eq!(v["sessionId"], "s1");
    }

    #[test]
    fn projects_subagent_child_variants() {
        let cases = [
            (
                agent::SubagentChildEvent::Text("reading files".into()),
                "text",
            ),
            (
                agent::SubagentChildEvent::Thinking("planning".into()),
                "thinking",
            ),
            (
                agent::SubagentChildEvent::ToolStart {
                    id: "t1".into(),
                    name: "Grep".into(),
                    hint: Some(("query".into(), "auth".into())),
                },
                "tool_start",
            ),
            (
                agent::SubagentChildEvent::ToolEnd {
                    id: "t1".into(),
                    name: "Grep".into(),
                    is_error: false,
                },
                "tool_end",
            ),
        ];
        for (child, kind) in cases {
            let json = thread_event_to_json(
                &ThreadEvent::SubagentChild {
                    id: "sub1".into(),
                    child,
                },
                Some("s1"),
            )
            .unwrap();
            let v: Value = serde_json::from_str(&json).unwrap();
            assert_eq!(v["type"], "subagent_child");
            assert_eq!(v["id"], "sub1");
            assert_eq!(v["event"]["kind"], kind);
            assert_eq!(v["sessionId"], "s1");
        }
    }
}
