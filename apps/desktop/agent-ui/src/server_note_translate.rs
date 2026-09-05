//! Reverse translation: `ServerNote` / `ServerCall` → `ThreadEvent`.
//!
//! Post-T10c (§D.6) only the retained `ServerNote` surface reaches this
//! module: the session-domain note arms are gone, and the v2 successor for
//! their events is [`crate::journal_translate`] (journal rows → the same
//! `ThreadEvent` vocabulary). `server_call_to_thread_event` stays: the
//! adjudication waterfall (Approve / AskUserQuestion / PlanVerdict) still
//! rides `ServerCall`.

use manox_agent::ThreadEvent;
use manox_protocol::{ServerCall, ServerNote};

/// Project a retained `ServerNote` onto the `ThreadEvent` the desktop
/// conversation renders. `None` for everything else (session lifecycle, the
/// transitional registry-push list channel, model-chat side stream — the
/// store / sidebar / model-chat paths mirror those).
pub fn server_note_to_thread_event(note: &ServerNote) -> Option<ThreadEvent> {
    use ServerNote::*;
    Some(match note {
        Error { message, .. } => ThreadEvent::Error(anyhow::anyhow!("{}", message)),
        // No ThreadEvent counterpart: owner control, the list channel, the
        // model-chat side stream.
        Ready
        | SessionCreated { .. }
        | SessionDisposed { .. }
        | ThreadsUpdated { .. }
        | Models { .. }
        | Commands { .. }
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
            tool_name: manox_agent::tools::ASK_USER_QUESTION.to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn retained_global_notes_have_no_event() {
        let notes = [
            ServerNote::Ready,
            ServerNote::SessionCreated {
                session_id: "s1".into(),
            },
            ServerNote::SessionDisposed {
                session_id: "s1".into(),
            },
            ServerNote::ThreadsUpdated { threads: vec![] },
            ServerNote::Models { models: vec![] },
            ServerNote::Commands {
                commands: serde_json::json!([]),
            },
            ServerNote::ModelText {
                request_id: "r".into(),
                text: "t".into(),
            },
            ServerNote::ModelChatDone {
                request_id: "r".into(),
                stop: None,
                error: None,
            },
        ];
        assert!(
            notes
                .iter()
                .all(|n| server_note_to_thread_event(n).is_none()),
            "retained non-domain notes must not emit ThreadEvents"
        );
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
                if tool_name == manox_agent::tools::ASK_USER_QUESTION
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
    fn browser_op_has_no_event() {
        let call = ServerCall::BrowserOp {
            session_id: "s1".into(),
            op: serde_json::json!({}),
        };
        assert!(server_call_to_thread_event(&call).is_none());
    }
}
