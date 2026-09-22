//! The chat channel (`ahp-chat:/<id>`) — one manox **session** (a journal file).
//!
//! Everything the transcript needs lives here: completed turns, the active turn
//! with its ordered response parts (streaming text, reasoning, tool calls), the
//! steering/queued messages and the composer draft. Branching maps onto AHP's
//! chat forking, so `createChat { source: fork|sideChat }` is how a manox fork
//! reaches a client.

use ahp_types::state::{ChatState, ChatSummary, SessionStatus};

/// `ahp-chat:/<id>`.
pub fn uri(id: &str) -> String {
    format!("ahp-chat:/{id}")
}

/// The chat id inside an `ahp-chat:/<id>` URI.
pub fn id(uri: &str) -> Option<&str> {
    uri.strip_prefix("ahp-chat:/").filter(|id| !id.is_empty())
}

/// An empty, idle chat.
pub fn initial(id: &str) -> ChatState {
    ChatState {
        resource: uri(id),
        title: String::new(),
        status: SessionStatus::Idle.bits(),
        activity: None,
        modified_at: super::now_iso8601(),
        origin: None,
        interactivity: None,
        working_directories: None,
        turns: Vec::new(),
        turns_next_cursor: None,
        active_turn: None,
        steering_message: None,
        queued_messages: None,
        draft: None,
        meta: None,
    }
}

/// The catalogue entry mirrored into the owning session's `chats`.
pub fn summary(state: &ChatState) -> ChatSummary {
    ChatSummary {
        resource: state.resource.clone(),
        title: state.title.clone(),
        status: state.status,
        activity: state.activity.clone(),
        modified_at: state.modified_at.clone(),
        origin: state.origin.clone(),
        interactivity: state.interactivity,
        working_directories: state.working_directories.clone(),
    }
}
