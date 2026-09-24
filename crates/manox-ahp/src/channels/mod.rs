//! Channel URIs and the host's authoritative channel state.
//!
//! AHP addresses every push interaction by channel URI, so this module is the
//! one place that knows the URI grammar and the one place that holds state per
//! channel. State is advanced exclusively through the SDK reducers
//! (`ahp::reducers`) — the same functions clients run — which is what makes
//! host/client convergence structural rather than aspirational.
//!
//! `x-manox-*` extension channels are *stateless* topics: AHP's `SnapshotState`
//! has no generic slot for private state, so their content travels as extension
//! action envelopes and a subscriber's baseline is pushed right after
//! `subscribe` (see `ext::actions`).

pub mod chat;
pub mod root;
pub mod session;
pub mod terminal;

use std::collections::HashMap;

use ahp::reducers::ReduceOutcome;
use ahp_types::actions::StateAction;
use ahp_types::common::Uri;
use ahp_types::state::{ChatState, RootState, SessionState, SnapshotState, TerminalState};

use crate::ext;

/// A parsed channel URI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Channel {
    /// `ahp-root://` — the always-present global channel.
    Root,
    /// `ahp-session:/<id>` — one manox thread.
    Session(String),
    /// `ahp-chat:/<id>` — one manox session (a journal).
    Chat(String),
    /// `ahp-terminal:/<id>` — one terminal.
    Terminal(String),
    /// `x-manox-*` extension channel, carried verbatim.
    Extension(String),
}

impl Channel {
    /// The channel's URI.
    pub fn uri(&self) -> String {
        match self {
            Self::Root => root::URI.to_string(),
            Self::Session(id) => session::uri(id),
            Self::Chat(id) => chat::uri(id),
            Self::Terminal(id) => terminal::uri(id),
            Self::Extension(uri) => uri.clone(),
        }
    }

    /// Whether the channel carries state, i.e. whether `subscribe` answers with
    /// a snapshot. Extension channels are stateless topics.
    pub fn is_state_bearing(&self) -> bool {
        !matches!(self, Self::Extension(_))
    }
}

/// Parse a channel URI. `None` for an unknown scheme: AHP says clients MUST NOT
/// subscribe to schemes they do not understand, and the host answers the same.
pub fn parse(uri: &str) -> Option<Channel> {
    if uri == root::URI {
        return Some(Channel::Root);
    }
    if let Some(id) = session::id(uri) {
        return Some(Channel::Session(id.to_string()));
    }
    if let Some(id) = chat::id(uri) {
        return Some(Channel::Chat(id.to_string()));
    }
    if let Some(id) = terminal::id(uri) {
        return Some(Channel::Terminal(id.to_string()));
    }
    if ext::channels::ALL
        .iter()
        .any(|prefix| uri.starts_with(prefix))
    {
        return Some(Channel::Extension(uri.to_string()));
    }
    None
}

/// The host's state for every state-bearing channel it serves.
///
/// Seeded from the backend (which folds the durable journal) and afterwards
/// advanced by the very action envelopes that go out on the wire.
pub struct ChannelStore {
    root: RootState,
    sessions: HashMap<String, SessionState>,
    chats: HashMap<String, ChatState>,
    /// chat id → owning session id (kept in step with each session's `chats`).
    chat_owner: HashMap<String, String>,
    terminals: HashMap<String, TerminalState>,
    /// `x-manox-*` channels: their state has no slot in AHP's `SnapshotState`
    /// (nine arms, no generic one), so it is folded here and delivered to
    /// subscribers as extension action envelopes.
    extensions: HashMap<String, crate::ext::XManoxState>,
}

impl ChannelStore {
    /// A store whose root channel starts from `root`.
    pub fn new(root: RootState) -> Self {
        Self {
            root,
            sessions: HashMap::new(),
            chats: HashMap::new(),
            chat_owner: HashMap::new(),
            terminals: HashMap::new(),
            extensions: HashMap::new(),
        }
    }

    /// The folded `x-manox` state of one extension channel, if any.
    pub fn extension(&self, uri: &str) -> Option<&crate::ext::XManoxState> {
        self.extensions.get(uri)
    }

    /// The root state (always present).
    pub fn root(&self) -> &RootState {
        &self.root
    }

    /// The snapshot a subscriber receives, if this channel carries state.
    pub fn snapshot(&self, channel: &Channel) -> Option<SnapshotState> {
        match channel {
            Channel::Root => Some(SnapshotState::Root(Box::new(self.root.clone()))),
            Channel::Session(id) => self
                .sessions
                .get(id)
                .map(|state| SnapshotState::Session(Box::new(state.clone()))),
            Channel::Chat(id) => self
                .chats
                .get(id)
                .map(|state| SnapshotState::Chat(Box::new(state.clone()))),
            Channel::Terminal(id) => self
                .terminals
                .get(id)
                .map(|state| SnapshotState::Terminal(Box::new(state.clone()))),
            Channel::Extension(_) => None,
        }
    }

    /// Apply one action to the channel's state with the SDK reducer.
    pub fn apply(&mut self, channel: &Channel, action: &StateAction) -> ReduceOutcome {
        let outcome = match channel {
            Channel::Root => ahp::reducers::apply_action_to_root(&mut self.root, action),
            Channel::Session(id) => match self.sessions.get_mut(id) {
                Some(state) => ahp::reducers::apply_action_to_session(state, action),
                None => ReduceOutcome::OutOfScope,
            },
            Channel::Chat(id) => match self.chats.get_mut(id) {
                Some(state) => ahp::reducers::apply_action_to_chat(state, action),
                None => ReduceOutcome::OutOfScope,
            },
            Channel::Terminal(id) => match self.terminals.get_mut(id) {
                Some(state) => ahp::reducers::apply_action_to_terminal(state, action),
                None => ReduceOutcome::OutOfScope,
            },
            // Extension state is ours: AHP's reducers ignore it by construction
            // (`StateAction::Unknown` is `OutOfScope` everywhere upstream), so
            // the fold lives in `ext::reducer` and the outcome is reported as a
            // plain `Applied`/`NoOp` here.
            Channel::Extension(uri) => {
                let entry = self.extensions.entry(uri.clone()).or_default();
                match crate::ext::reducer::apply(
                    entry,
                    &serde_json::to_value(action).unwrap_or_default(),
                ) {
                    crate::ext::ExtOutcome::Applied => ReduceOutcome::Applied,
                    crate::ext::ExtOutcome::NoOp => ReduceOutcome::NoOp,
                    crate::ext::ExtOutcome::Unrecognised => ReduceOutcome::NoOp,
                }
            }
        };
        if let Channel::Session(id) = channel {
            self.reindex_chats(id);
        }
        outcome
    }

    /// Seed (or replace) one session's state.
    pub fn insert_session(&mut self, id: &str, state: SessionState) {
        self.sessions.insert(id.to_string(), state);
        self.reindex_chats(id);
    }

    /// Seed (or replace) one chat's state and its session link.
    pub fn insert_chat(&mut self, session_id: &str, id: &str, state: ChatState) {
        self.chats.insert(id.to_string(), state);
        self.chat_owner
            .insert(id.to_string(), session_id.to_string());
    }

    /// Seed (or replace) one terminal's state.
    /// Drop a terminal's state (its PTY is the backend's to release).
    pub fn remove_terminal(&mut self, id: &str) {
        self.terminals.remove(id);
    }

    pub fn insert_terminal(&mut self, id: &str, state: TerminalState) {
        self.terminals.insert(id.to_string(), state);
    }

    /// Drop a session and every chat it owned.
    pub fn remove_session(&mut self, id: &str) {
        self.sessions.remove(id);
        let orphans: Vec<String> = self
            .chat_owner
            .iter()
            .filter(|(_, owner)| owner.as_str() == id)
            .map(|(chat, _)| chat.clone())
            .collect();
        for chat in orphans {
            self.remove_chat(&chat);
        }
    }

    /// Drop a chat.
    pub fn remove_chat(&mut self, id: &str) {
        self.chats.remove(id);
        self.chat_owner.remove(id);
    }

    /// One session's state.
    pub fn session(&self, id: &str) -> Option<&SessionState> {
        self.sessions.get(id)
    }

    /// One chat's state.
    pub fn chat(&self, id: &str) -> Option<&ChatState> {
        self.chats.get(id)
    }

    /// One terminal's state.
    pub fn terminal(&self, id: &str) -> Option<&TerminalState> {
        self.terminals.get(id)
    }

    /// The session owning `chat_id`.
    pub fn chat_session(&self, chat_id: &str) -> Option<&str> {
        self.chat_owner.get(chat_id).map(String::as_str)
    }

    /// Known session ids.
    pub fn session_ids(&self) -> Vec<String> {
        self.sessions.keys().cloned().collect()
    }

    fn reindex_chats(&mut self, session_id: &str) {
        let Some(state) = self.sessions.get(session_id) else {
            return;
        };
        let owned: Vec<Uri> = state
            .chats
            .iter()
            .map(|chat| chat.resource.clone())
            .collect();
        self.chat_owner
            .retain(|_, owner| owner.as_str() != session_id);
        for uri in owned {
            if let Some(id) = chat::id(&uri) {
                self.chat_owner
                    .insert(id.to_string(), session_id.to_string());
            }
        }
    }
}

/// Current wall-clock stamp in the shape AHP uses for `createdAt` /
/// `modifiedAt` (`2025-03-10T18:42:03.123Z`).
pub fn now_iso8601() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_declared_scheme() {
        assert_eq!(parse("ahp-root://"), Some(Channel::Root));
        assert_eq!(
            parse("ahp-session:/s-1"),
            Some(Channel::Session("s-1".into()))
        );
        assert_eq!(parse("ahp-chat:/c-1"), Some(Channel::Chat("c-1".into())));
        assert_eq!(
            parse("ahp-terminal:/t-1"),
            Some(Channel::Terminal("t-1".into()))
        );
        assert_eq!(
            parse("x-manox-plan:/c-1"),
            Some(Channel::Extension("x-manox-plan:/c-1".into()))
        );
        assert_eq!(parse("ahp-changeset:/x"), None);
        assert!(!Channel::Extension("x".into()).is_state_bearing());
    }

    #[test]
    fn chat_ownership_follows_the_session_catalog() {
        use ahp_types::state::{ChatSummary, SessionLifecycle};
        let mut store = ChannelStore::new(root::empty());
        let chat_uri = chat::uri("c-1");
        let mut state = session::initial("manox", None, None);
        state.lifecycle = SessionLifecycle::Ready;
        state.chats.push(ChatSummary {
            resource: chat_uri.clone(),
            title: "main".into(),
            status: 1,
            activity: None,
            modified_at: now_iso8601(),
            origin: None,
            interactivity: None,
            working_directories: None,
        });
        store.insert_session("s-1", state);
        assert_eq!(store.chat_session("c-1"), Some("s-1"));
        store.remove_session("s-1");
        assert_eq!(store.chat_session("c-1"), None);
    }
}
