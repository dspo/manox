//! The session channel (`ahp-session:/<id>`) — one manox **thread**.
//!
//! A thread is the sidebar row: a title, a project, the granted working
//! directories, the pin/archive bits and the catalogue of journals (AHP chats)
//! it owns. `defaultChat` is the thread's active-session pointer, so switching
//! the active session is a `session/defaultChatChanged` action rather than a
//! bespoke command.

use ahp_types::common::Uri;
use ahp_types::state::{
    ProjectInfo, SessionConfigState, SessionLifecycle, SessionState, SessionSummary,
};

/// `ahp-session:/<id>`.
pub fn uri(id: &str) -> String {
    format!("ahp-session:/{id}")
}

/// The session id inside an `ahp-session:/<id>` URI.
pub fn id(uri: &str) -> Option<&str> {
    uri.strip_prefix("ahp-session:/")
        .filter(|id| !id.is_empty())
}

/// A `lifecycle: creating` session state, as `createSession` must answer before
/// the backend finishes initialising.
/// An empty session state for a session with no store row yet.
///
/// `createSession` seeds the session it just created, and a brand-new session's
/// row only appears after a list refresh scans its file. An empty state is what
/// that session *is* at that moment — answering "not found" for it would make
/// the create command fail on its own subject.
pub fn initial_empty(thread_id: &str) -> SessionState {
    let mut state = initial("", None, None);
    // The active-session pointer is the one thing a store row would have told
    // us, and it has a correct default for a fresh session: the session itself.
    state.default_chat = Some(crate::channels::chat::uri(thread_id));
    state
}

pub fn initial(
    provider: &str,
    working_directories: Option<Vec<Uri>>,
    config: Option<SessionConfigState>,
) -> SessionState {
    SessionState {
        provider: provider.to_string(),
        title: String::new(),
        status: ahp_types::state::SessionStatus::Idle.bits(),
        activity: None,
        origin: None,
        project: None,
        working_directories,
        annotations: None,
        lifecycle: SessionLifecycle::Creating,
        creation_error: None,
        server_tools: None,
        active_clients: Vec::new(),
        chats: Vec::new(),
        default_chat: None,
        config,
        customizations: None,
        changesets: None,
        input_needed: None,
        meta: None,
    }
}

/// The lightweight catalogue entry mirrored into the root channel.
pub fn summary(
    state: &SessionState,
    id: &str,
    created_at: &str,
    modified_at: &str,
    status: u32,
) -> SessionSummary {
    SessionSummary {
        provider: state.provider.clone(),
        title: state.title.clone(),
        status,
        activity: state.activity.clone(),
        origin: None,
        project: state.project.clone(),
        working_directories: state.working_directories.clone(),
        annotations: None,
        resource: uri(id),
        created_at: created_at.to_string(),
        modified_at: modified_at.to_string(),
        changes: None,
        meta: None,
    }
}

/// A project binding for a session.
pub fn project(uri: &str, display_name: &str) -> ProjectInfo {
    ProjectInfo {
        uri: uri.to_string(),
        display_name: display_name.to_string(),
    }
}
