//! Shared test fixtures: a minimal but honest [`Backend`].
#![allow(dead_code)] // every integration-test binary includes this module and uses a different subset
//!
//! The backend answers from an in-memory stand-in for the durable journal, so
//! the conformance suites exercise the real host path (route → acceptance →
//! reducer → broadcast) without dragging the runtime in.

use std::collections::BTreeMap;
use std::sync::Arc;

use ahp_types::actions::{ActionOrigin, StateAction};
use ahp_types::commands::CreateChatParams;
use ahp_types::commands::CreateSessionParams;
use ahp_types::state::{
    AgentInfo, ChatState, RootState, SessionLifecycle, SessionModelInfo, SessionState,
    SessionSummary, TerminalState, Turn,
};
use manox_ahp::backend::{Backend, DispatchOutcome};
use manox_ahp::channels::{chat, root, session};
use manox_ahp::error::HostError;
use serde_json::Value;

/// A backend holding `session id → chat id` for one seeded session.
pub struct TestBackend {
    sessions: parking_lot::Mutex<BTreeMap<String, String>>,
}

impl TestBackend {
    /// A backend with session `s-1` owning chat `c-1`.
    pub fn new() -> Arc<Self> {
        let mut sessions = BTreeMap::new();
        sessions.insert("s-1".to_string(), "c-1".to_string());
        Arc::new(Self {
            sessions: parking_lot::Mutex::new(sessions),
        })
    }

    /// The seeded chat of `session_id`, taking no lock itself (callers hold
    /// it) — the fixture's map is not reentrant, mirroring the L1 rule.
    fn chat_of_locked(sessions: &BTreeMap<String, String>, session_id: &str) -> Option<String> {
        sessions.get(session_id).cloned()
    }

    /// The session URI of the seeded session.
    pub fn session_uri() -> String {
        session::uri("s-1")
    }

    /// The chat URI of the seeded chat.
    pub fn chat_uri() -> String {
        chat::uri("c-1")
    }
}

impl Backend for TestBackend {
    fn root_state(&self) -> RootState {
        root::with_agents(vec![agent()], None)
    }

    fn list_sessions(&self) -> Vec<SessionSummary> {
        let sessions = self.sessions.lock();
        sessions
            .iter()
            .map(|(id, chat_id)| summary_of(id, chat_id))
            .collect()
    }

    fn session_summary(&self, session_id: &str) -> Option<SessionSummary> {
        let sessions = self.sessions.lock();
        let chat_id = Self::chat_of_locked(&sessions, session_id)?;
        Some(summary_of(session_id, &chat_id))
    }

    fn session_state(&self, session_id: &str) -> Option<SessionState> {
        let chat_id = {
            let sessions = self.sessions.lock();
            Self::chat_of_locked(&sessions, session_id)?
        };
        Some(session_state_with(&chat_id))
    }

    fn chat_state(&self, chat_id: &str) -> Option<ChatState> {
        let owned = self.sessions.lock().values().any(|id| id == chat_id);
        owned.then(|| chat::initial(chat_id))
    }

    fn session_state_for_chat(&self, chat_id: &str) -> Option<String> {
        self.sessions
            .lock()
            .iter()
            .find(|(_, chat)| chat.as_str() == chat_id)
            .map(|(session, _)| session.clone())
    }

    fn terminal_state(&self, _terminal_id: &str) -> Option<TerminalState> {
        None
    }

    fn create_session(
        &self,
        session_id: &str,
        _params: &CreateSessionParams,
    ) -> Result<(), HostError> {
        self.sessions
            .lock()
            .insert(session_id.to_string(), format!("{session_id}-chat"));
        Ok(())
    }

    fn dispose_session(&self, session_id: &str) -> Result<(), HostError> {
        self.sessions.lock().remove(session_id);
        Ok(())
    }

    fn create_chat(
        &self,
        session_id: &str,
        chat_id: &str,
        _params: &CreateChatParams,
    ) -> Result<(), HostError> {
        self.sessions
            .lock()
            .insert(session_id.to_string(), chat_id.to_string());
        Ok(())
    }

    fn dispose_chat(&self, chat_id: &str) -> Result<(), HostError> {
        self.sessions.lock().retain(|_, chat| chat != chat_id);
        Ok(())
    }

    fn fetch_turns(
        &self,
        _chat_id: &str,
        _cursor: Option<&str>,
        _limit: Option<i64>,
    ) -> Result<Vec<Turn>, HostError> {
        Ok(Vec::new())
    }

    fn dispatch(
        &self,
        _channel: &str,
        _action: &StateAction,
        _origin: &ActionOrigin,
    ) -> DispatchOutcome {
        DispatchOutcome::Accepted
    }

    fn extension(&self, _method: &str, _params: &Value) -> Result<Value, HostError> {
        Err(HostError::Unimplemented("extension".to_string()))
    }
}

/// The seeded session state for `chat_id`.
fn session_state_with(chat_id: &str) -> SessionState {
    let mut state = session::initial("manox", None, None);
    state.title = "seeded".to_string();
    state.lifecycle = SessionLifecycle::Ready;
    state.chats = vec![chat::summary(&chat::initial(chat_id))];
    state.default_chat = Some(chat::uri(chat_id));
    state
}

/// The catalogue entry for one seeded session.
fn summary_of(session_id: &str, chat_id: &str) -> SessionSummary {
    let state = session_state_with(chat_id);
    session::summary(
        &state,
        session_id,
        "2026-09-23T00:00:00.000Z",
        "2026-09-23T00:00:00.000Z",
        state.status,
    )
}

/// One agent with one model, as the root channel would advertise it.
pub fn agent() -> AgentInfo {
    AgentInfo {
        provider: "manox".to_string(),
        display_name: "manox".to_string(),
        description: "local runtime".to_string(),
        models: vec![SessionModelInfo {
            id: "anthropic-main/claude-sonnet-4".to_string(),
            provider: "anthropic-main".to_string(),
            name: "Claude Sonnet 4".to_string(),
            max_context_window: Some(200_000),
            max_output_tokens: None,
            max_prompt_tokens: None,
            supports_vision: Some(true),
            policy_state: None,
            config_schema: None,
            meta: None,
        }],
        protected_resources: None,
        customizations: None,
        capabilities: None,
    }
}

/// Build an action from its wire JSON — the same shape a client would send.
pub fn action(json: Value) -> StateAction {
    serde_json::from_value(json).expect("known action shape")
}

/// The one scenario both transports run: subscribe, publish, dispatch,
/// unsubscribe. The returned log is compared against [`EXPECTED_LOG`] by the
/// in-process and WebSocket suites alike.
pub async fn run_scenario<T: ahp::Transport>(host: manox_ahp::Host, transport: T) -> Vec<String> {
    use ahp::{Client, ClientConfig};
    use ahp_types::common::ROOT_RESOURCE_URI;
    use ahp_types::version::PROTOCOL_VERSION;
    use std::time::Duration;

    let client = Client::connect(transport, ClientConfig::default())
        .await
        .expect("connects");
    client
        .initialize(
            "scenario".to_string(),
            vec![PROTOCOL_VERSION.to_string()],
            vec![ROOT_RESOURCE_URI.to_string(), TestBackend::session_uri()],
        )
        .await
        .expect("initializes");
    let mut session_sub = client
        .attach_subscription(&TestBackend::session_uri())
        .await;
    let (_result, mut chat_sub) = client
        .subscribe(TestBackend::chat_uri())
        .await
        .expect("subscribes to the chat");

    let mut log = Vec::new();

    host.publish(
        &TestBackend::session_uri(),
        action(serde_json::json!({"type": "session/titleChanged", "title": "scenario"})),
        None,
    );
    let envelope = next_action(&mut session_sub).await;
    log.push(format!("{} {}", envelope.server_seq, tag(&envelope.action)));

    client
        .dispatch(
            TestBackend::chat_uri(),
            action(serde_json::json!({
                "type": "chat/pendingMessageRemoved",
                "kind": "steering",
                "id": "p-1",
            })),
        )
        .await
        .expect("dispatches");
    let envelope = next_action(&mut chat_sub).await;
    log.push(format!(
        "{} {} {}",
        envelope.server_seq,
        tag(&envelope.action),
        envelope.origin.expect("echo carries origin").client_seq,
    ));

    client
        .unsubscribe(TestBackend::session_uri())
        .await
        .expect("unsubscribes");
    host.publish(
        &TestBackend::session_uri(),
        action(serde_json::json!({"type": "session/titleChanged", "title": "after unsubscribe"})),
        None,
    );
    let silent = tokio::time::timeout(Duration::from_millis(300), session_sub.recv()).await;
    log.push(match silent {
        Ok(None) => "unsubscribed".to_string(),
        Ok(Some(other)) => format!("unexpected delivery: {other:?}"),
        Err(_) => "unexpected silence".to_string(),
    });

    log
}

/// The expected log for [`run_scenario`], shared by both transports.
pub const EXPECTED_LOG: &[&str] = &[
    "1 session/titleChanged",
    "2 chat/pendingMessageRemoved 1",
    "unsubscribed",
];

async fn next_action(sub: &mut ahp::SessionSubscription) -> ahp_types::actions::ActionEnvelope {
    use std::time::Duration;

    let event = tokio::time::timeout(Duration::from_secs(2), sub.recv())
        .await
        .expect("action arrives")
        .expect("subscription open");
    match event {
        ahp::SubscriptionEvent::Action(envelope) => envelope,
        other => panic!("expected an action envelope, got {other:?}"),
    }
}

/// The wire tag of an action.
pub fn tag(action: &StateAction) -> String {
    serde_json::to_value(action)
        .ok()
        .and_then(|value| value["type"].as_str().map(str::to_string))
        .expect("actions serialize with a type tag")
}
