//! The runtime behind the AHP host: seeding, live bridging, and the write seam.
//!
//! Two responsibilities, both thin by design:
//!
//! - **Seeding / live bridge.** A session's AHP state is the deterministic fold of
//!   its journal ([`super::chat_state`] / [`super::session_state`]). The bridge
//!   takes that fold, installs it on the host, then forwards every journal feed
//!   event *after* the fold's tail through the same translator and reducers the
//!   client runs — so the host never re-states what the seed already carried and
//!   the client's snapshot-then-deltas stream stays convergent.
//! - **Writes.** `dispatch` maps an accepted AHP action onto the *existing*
//!   runtime intents (`AgentServerInner::submit` and friends) rather than a second
//!   implementation of the turn loop. Anything not wired yet is refused loudly, as
//!   the trait contract requires: a refused write must never look accepted.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock, Weak};

use ahp_types::actions::{ActionOrigin, StateAction};
use ahp_types::commands::{CreateChatParams, CreateSessionParams};
use ahp_types::state::{
    AgentInfo, ChatState, RootState, SessionModelInfo, SessionState, SessionSummary, TerminalState,
    Turn,
};
use manox_ahp::backend::{Backend, DispatchOutcome};
use manox_ahp::channels::{chat, root, session};
use manox_ahp::error::HostError;
use manox_ahp::translate::Translator;
use manox_journal::JournalWireEvent;
use parking_lot::Mutex;
use serde_json::Value;

use super::fold_journal;

/// Run an async runtime seam from the host's synchronous `Backend` methods.
///
/// The host calls seeding from an async context but the trait is synchronous, so
/// the block has to yield the worker rather than park it (`block_in_place`),
/// which keeps the awaited runtime task able to make progress.
fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::task::block_in_place(|| manox_agent::runtime::handle().block_on(future))
}
use crate::agent_server::AgentServer;

/// One session's folded state plus the journal tail it was taken at.
struct Seeded {
    thread_id: String,
    session: SessionState,
    chat: ChatState,
    tail: u64,
}

/// The runtime adapter the AHP host talks to.
pub(crate) struct RuntimeBackend {
    server: Arc<AgentServer>,
    cwd: PathBuf,
    /// Set once by the runtime right after `Host::new`: the bridge needs the host
    /// to publish, and the host needs the backend to seed — a cycle broken by
    /// handing the weak reference over after both exist.
    host: OnceLock<Weak<manox_ahp::Host>>,
    seeds: Mutex<HashMap<String, Arc<Seeded>>>,
    bridges: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
    /// The bridge task needs an `Arc` of this backend while the host holds only
    /// `&self` through the trait, so the backend keeps a weak handle to itself.
    me: OnceLock<Weak<RuntimeBackend>>,
}

impl RuntimeBackend {
    pub(crate) fn new(server: Arc<AgentServer>, cwd: PathBuf) -> Arc<Self> {
        let backend = Arc::new(Self {
            server,
            cwd,
            host: OnceLock::new(),
            seeds: Mutex::new(HashMap::new()),
            bridges: Mutex::new(HashMap::new()),
            me: OnceLock::new(),
        });
        let _ = backend.me.set(Arc::downgrade(&backend));
        backend
    }

    fn me(&self) -> Option<Arc<Self>> {
        self.me.get().and_then(Weak::upgrade)
    }

    pub(crate) fn attach_host(&self, host: &Arc<manox_ahp::Host>) {
        let _ = self.host.set(Arc::downgrade(host));
    }

    fn host(&self) -> Option<Arc<manox_ahp::Host>> {
        self.host.get().and_then(Weak::upgrade)
    }

    /// The folded state of one session, seeding the host and starting the bridge
    /// on first sight.
    async fn seeded(&self, session_id: &str) -> Option<Arc<Seeded>> {
        if let Some(seeded) = self.seeds.lock().get(session_id).cloned() {
            return Some(seeded);
        }
        let thread_id = super::thread_of_session(session_id).await?;
        let fold = fold_journal(session_id, &thread_id).await?;
        let session_state = super::session_state(&thread_id).await?;
        let seeded = Arc::new(Seeded {
            thread_id,
            session: session_state,
            chat: fold.chat,
            tail: fold.tail,
        });
        self.seeds
            .lock()
            .insert(session_id.to_string(), Arc::clone(&seeded));
        if let Some(host) = self.host() {
            host.seed_session(&seeded.thread_id, seeded.session.clone());
            host.seed_chat(&seeded.thread_id, session_id, seeded.chat.clone());
            self.ensure_bridge(session_id);
        }
        Some(seeded)
    }

    /// Start the live bridge for `session_id` (idempotent).
    fn ensure_bridge(&self, session_id: &str) {
        let mut bridges = self.bridges.lock();
        if bridges.contains_key(session_id) {
            return;
        }
        let Some(thread) = self.server.ahp_inner().session_thread(session_id) else {
            // A cold session (no live engine) has no feed to bridge; its state
            // still answers from the journal, and a submit materializes the
            // engine, which re-runs this path.
            return;
        };
        let Some(host) = self.host() else {
            return;
        };
        let Some(backend) = self.me() else {
            return;
        };
        let id = session_id.to_string();
        let task = manox_agent::runtime::handle().spawn(async move {
            backend.bridge(host, thread, id).await;
        });
        bridges.insert(session_id.to_string(), task);
    }

    /// Forward journal feed events into the host, from the seeded tail onward.
    ///
    /// A lagged feed (the kernel's bounded window) is a resync: re-fold, re-seed
    /// the host state from the journal, and continue above the new tail. The fold
    /// is deterministic, so a resync cannot invent state — it restates what the
    /// journal already says.
    async fn bridge(
        &self,
        host: Arc<manox_ahp::Host>,
        thread: manox_agent::thread::ThreadHandle,
        session_id: String,
    ) {
        let mut feed = thread.subscribe_journal_feed();
        let mut translator = Translator::new();
        let mut tail = self
            .seeds
            .lock()
            .get(&session_id)
            .map(|seeded| seeded.tail)
            .unwrap_or_default();
        loop {
            match feed.recv().await {
                Ok(manox_agent::engine::JournalFeed::Event(event)) => {
                    if event.seq <= tail {
                        continue;
                    }
                    tail = event.seq;
                    let Some(entry) = crate::translate::wire_entry(event.seq, &event.entry) else {
                        continue;
                    };
                    let thread_id = self
                        .seeds
                        .lock()
                        .get(&session_id)
                        .map(|seeded| seeded.thread_id.clone())
                        .unwrap_or_else(|| session_id.clone());
                    for emitted in translator.on_entry(&session_id, &thread_id, &entry) {
                        host.publish(&emitted.channel, emitted.action, None);
                    }
                }
                Ok(manox_agent::engine::JournalFeed::Lagged(_)) => {
                    translator = Translator::new();
                    if let Some(fold) = fold_journal(&session_id, &session_id).await {
                        let Some(session_state) = super::session_state(&session_id).await else {
                            continue;
                        };
                        tail = fold.tail;
                        host.seed_chat(&session_id, &session_id, fold.chat);
                        host.seed_session(&session_id, session_state);
                    }
                }
                Err(_) => break,
            }
        }
    }

    /// The agent catalogue for the root channel, from the live provider registry
    /// (the same source `ListModels` answers from).
    fn agents(&self) -> Vec<AgentInfo> {
        let registry = manox_agent::provider_glue::global();
        registry
            .provider_names()
            .into_iter()
            .map(|provider| {
                let models = registry
                    .models()
                    .into_iter()
                    .filter(|model| model.provider == provider)
                    .map(|model| SessionModelInfo {
                        // L8: the wire never carries a bare model id.
                        id: format!("{}/{}", model.provider, model.id),
                        provider: model.provider.clone(),
                        name: model.id.clone(),
                        max_context_window: Some(model.context_window as i64),
                        max_output_tokens: Some(model.max_tokens as i64),
                        max_prompt_tokens: None,
                        supports_vision: None,
                        policy_state: None,
                        config_schema: None,
                        meta: None,
                    })
                    .collect();
                AgentInfo {
                    provider: provider.clone(),
                    display_name: provider.clone(),
                    description: String::new(),
                    models,
                    protected_resources: None,
                    customizations: None,
                    capabilities: Some(ahp_types::state::AgentCapabilities {
                        multiple_chats: Some(ahp_types::state::MultipleChatsCapability {
                            fork: Some(true),
                            side_chat: Some(true),
                        }),
                        multiple_working_directories: Some(
                            ahp_types::state::MultipleWorkingDirectoriesCapability {
                                immutable_primary: Some(false),
                                primary_replacement: None,
                            },
                        ),
                    }),
                }
            })
            .collect()
    }
}

impl Backend for RuntimeBackend {
    fn root_state(&self) -> RootState {
        root::with_agents(self.agents(), None)
    }

    fn list_sessions(&self) -> Vec<SessionSummary> {
        let Some(store) = manox_agent::thread_store::try_global() else {
            return Vec::new();
        };
        let ids = store.read(|state| {
            state
                .summaries()
                .iter()
                .map(|row| row.id.clone())
                .collect::<Vec<_>>()
        });
        let mut out = Vec::new();
        for id in ids {
            if let Some(summary) = self.session_summary(&id) {
                out.push(summary);
            }
        }
        out
    }

    fn session_summary(&self, session_id: &str) -> Option<SessionSummary> {
        let store = manox_agent::thread_store::try_global()?;
        let row = store.read(|state| state.summary_by_id(session_id).cloned())?;
        // The store row carries no timestamps; the journal's own last entry is
        // the deterministic stamp available to a fold (a wall-clock stamp here
        // would make the fold non-reproducible).
        let stamp = self
            .seeds
            .lock()
            .get(session_id)
            .map(|seeded| seeded.chat.modified_at.clone())
            .unwrap_or_default();
        let state = block_on(super::session_state(session_id))?;
        let mut summary = session::summary(&state, session_id, &stamp, &stamp, state.status);
        summary.provider = row.provider_id.unwrap_or(summary.provider);
        Some(summary)
    }

    fn session_state(&self, session_id: &str) -> Option<SessionState> {
        let seeded = block_on(self.seeded(session_id))?;
        Some(seeded.session.clone())
    }

    fn chat_state(&self, chat_id: &str) -> Option<ChatState> {
        let seeded = block_on(self.seeded(chat_id))?;
        Some(seeded.chat.clone())
    }

    fn session_state_for_chat(&self, chat_id: &str) -> Option<String> {
        block_on(super::thread_of_session(chat_id))
    }

    fn terminal_state(&self, _terminal_id: &str) -> Option<TerminalState> {
        // The AHP terminal channel is not part of this slice: the runtime's
        // terminals are followed through the v2 surface until the terminal plane
        // lands (an absent state answers `not found`, never a fabricated one).
        None
    }

    fn create_session(
        &self,
        session_id: &str,
        params: &CreateSessionParams,
    ) -> Result<(), HostError> {
        let working_directories: Vec<String> = params
            .working_directories
            .clone()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|uri| file_uri_to_path(&uri))
            .collect();
        let owner = params
            .active_client
            .as_ref()
            .map(|client| client.client_id.clone())
            .unwrap_or_else(|| "ahp".to_string());
        let intent = crate::agent_server::SessionIntent {
            session_id: Some(session_id.to_string()),
            cwd: working_directories.first().cloned(),
            project: None,
            initial_model: None,
            approval_mode: None,
            reasoning_effort: None,
            seed: None,
            working_directories,
        };
        let server = Arc::clone(&self.server);
        let inner = Arc::clone(server.ahp_inner());
        block_on(async move {
            crate::agent_server::AgentServerInner::create_session_request(&inner, &owner, intent)
                .await
        })
        .map(|_| ())
        .map_err(|error| HostError::Backend(error.message))
    }

    fn dispose_session(&self, session_id: &str) -> Result<(), HostError> {
        self.server.ahp_inner().dispose_session("ahp", session_id);
        Ok(())
    }

    fn create_chat(
        &self,
        _session_id: &str,
        _chat_id: &str,
        _params: &CreateChatParams,
    ) -> Result<(), HostError> {
        // A chat is a journal; branching one is `createChat{source}` mapped onto
        // the runtime's fork intent, which is not wired yet.
        Err(HostError::Unimplemented("createChat".to_string()))
    }

    fn dispose_chat(&self, _chat_id: &str) -> Result<(), HostError> {
        Err(HostError::Unimplemented("disposeChat".to_string()))
    }

    fn fetch_turns(
        &self,
        _chat_id: &str,
        _cursor: Option<&str>,
        _limit: Option<i64>,
    ) -> Result<Vec<Turn>, HostError> {
        // The subscription snapshot already carries the retained turns; paging
        // older ones into state needs the metrics/history plane.
        Ok(Vec::new())
    }

    fn dispatch(
        &self,
        channel: &str,
        action: &StateAction,
        origin: &ActionOrigin,
    ) -> DispatchOutcome {
        let Some(session_id) = chat::id(channel).map(str::to_string) else {
            return DispatchOutcome::Rejected(format!("no runtime intent for {channel}"));
        };
        match action {
            StateAction::ChatTurnStarted(started) => {
                let text = started.message.text.clone();
                let owner = origin.client_id.clone();
                let inner = Arc::clone(self.server.ahp_inner());
                match block_on(async move {
                    inner
                        .submit(&owner, &session_id, text, Vec::new(), None, None)
                        .await
                }) {
                    Ok(_) => DispatchOutcome::Accepted,
                    Err(error) => DispatchOutcome::Rejected(error.message),
                }
            }
            other => DispatchOutcome::Rejected(format!(
                "no runtime intent yet: {}",
                manox_ahp::wire::action_tag(other)
            )),
        }
    }

    fn extension(&self, method: &str, _params: &Value) -> Result<Value, HostError> {
        Err(HostError::Unimplemented(method.to_string()))
    }
}

/// `file://` URI → filesystem path (`None` for other schemes).
fn file_uri_to_path(uri: &str) -> Option<String> {
    uri.strip_prefix("file://").map(str::to_string)
}

/// The journal's own last-entry vocabulary marker, re-exported for the tests'
/// scripted journals.
#[allow(dead_code)]
pub(crate) fn is_journal_event(event: &JournalWireEvent) -> bool {
    !matches!(event, JournalWireEvent::Metrics { .. })
}

/// The runtime's working directory (kept for the root config once it lands).
#[allow(dead_code)]
pub(crate) fn cwd_of(backend: &RuntimeBackend) -> &std::path::Path {
    &backend.cwd
}
