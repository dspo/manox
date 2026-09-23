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

/// A snapshot cut to the trailing `turns` completed turns.
///
/// Journal-backed sessions can be enormous (a real one here is 67 MB / 272k
/// entries), and AHP delivers a snapshot in a single JSON-RPC message: an
/// uncut state would exceed what any client will frame and close the
/// connection. `view.turns` is the protocol's own answer — the client asks for
/// a tail and pages older turns in with `fetchTurns` — so the host serves the
/// tail and leaves `turnsNextCursor` pointing at the first turn it withheld.
///
/// The host's own state is never cut: this shapes one subscriber's snapshot.
pub fn tail_view(state: &ChatState, turns: Option<i64>, default_turns: usize) -> ChatState {
    let keep = match turns {
        Some(requested) if requested > 0 => requested as usize,
        // A client that asked for nothing specific still gets a bounded frame;
        // the cursor below is what keeps that lossless.
        _ => default_turns,
    };
    if state.turns.len() <= keep {
        return state.clone();
    }
    let mut cut = state.clone();
    let first_kept = state.turns.len() - keep;
    cut.turns_next_cursor = Some(cursor_of(first_kept));
    cut.turns = state.turns[first_kept..].to_vec();
    cut
}

/// The paging cursor addressing the turn at `index` (the first withheld one).
pub fn cursor_of(index: usize) -> String {
    format!("turn:{index}")
}

/// The index a [`cursor_of`] string addresses.
pub fn index_of(cursor: &str) -> Option<usize> {
    cursor.strip_prefix("turn:")?.parse().ok()
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
