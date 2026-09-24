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
use manox_ahp::resource::ResourcePlane;
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
    /// The `x-manox*` channel state this journal folds to, by channel URI.
    /// The AHP reducers do not fold extension actions, so this is what a
    /// subscriber to a declared extension channel is answered with instead of
    /// silence.
    extensions: HashMap<String, manox_ahp::ext::XManoxState>,
}

/// The notification carrying an extension channel's baseline state.
///
/// Named in the `x-manox` namespace: it is our surface, not AHP's.
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
    /// The `resource*` file plane, fenced to the runtime's working directory.
    resources: super::resources::RuntimeResources,
}

impl RuntimeBackend {
    pub(crate) fn new(server: Arc<AgentServer>, cwd: PathBuf) -> Arc<Self> {
        let resources = super::resources::RuntimeResources::new(vec![cwd.clone()]);
        let backend = Arc::new(Self {
            server,
            cwd,
            host: OnceLock::new(),
            seeds: Mutex::new(HashMap::new()),
            bridges: Mutex::new(HashMap::new()),
            me: OnceLock::new(),
            resources,
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
        let thread_id = super::thread_of_session(session_id)
            .await
            .unwrap_or_else(|| session_id.to_string());
        // A session created moments ago has no journal line yet: it is an empty
        // chat, not an unknown one. Subscribing to a brand-new session must work,
        // and the bridge picks its entries up from seq 0 onward.
        let (chat, tail, extensions) = match fold_journal(session_id, &thread_id).await {
            Some(fold) => {
                let mut extensions: HashMap<String, manox_ahp::ext::XManoxState> = HashMap::new();
                for (channel, action) in &fold.extension_actions {
                    let Ok(value) = serde_json::to_value(action) else {
                        continue;
                    };
                    let state = extensions.entry(channel.clone()).or_default();
                    let _ = manox_ahp::ext::reducer::apply(state, &value);
                }
                (fold.chat, fold.tail, extensions)
            }
            None => (chat::initial(session_id), 0, HashMap::new()),
        };
        // The session half needs the same tolerance as the chat half, for the
        // same reason: a brand-new session has no store row until a list
        // refresh scans its file, and `createSession` seeds it immediately —
        // its own step, before any refresh. Requiring the row there made
        // `createSession` answer `session not found` for the session it had just
        // created. An empty state is what the session *is* at this point, and the
        // fold replaces it as soon as the row appears.
        let session_state = match super::session_state(&thread_id).await {
            Some(state) => state,
            None => session::initial_empty(&thread_id),
        };
        let seeded = Arc::new(Seeded {
            thread_id,
            session: session_state,
            chat,
            tail,
            extensions,
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

    /// The live facts for a session this host has already folded, if any.
    fn seeded_status(&self, session_id: &str) -> Option<SeededFacts> {
        let seeded = self.seeds.lock().get(session_id).cloned()?;
        Some(SeededFacts {
            status: seeded.session.status,
            activity: seeded.session.activity.clone(),
            directories: seeded.session.working_directories.clone(),
            title: seeded.session.title.clone(),
        })
    }

    /// The baseline for a connection-level catalogue channel.
    ///
    /// `x-manox-workspaces://` and `x-manox-commands://` describe the host, not
    /// one session, so their state comes from the runtime's own tables rather
    /// than a journal fold. A channel we declare but cannot describe answers
    /// `Null` — deliberately still an answer, because the declaration is the
    /// contract and silence would be the one outcome a client cannot act on.
    fn catalogue_baseline(&self, channel: &str) -> Value {
        if channel.starts_with(manox_ahp::ext::channels::WORKSPACES) {
            let workspaces = manox_agent::thread_store::try_global()
                .map(|store| store.read(|state| state.known_projects().to_vec()))
                .unwrap_or_default();
            return serde_json::json!({ "workspaces": workspaces });
        }
        if channel.starts_with(manox_ahp::ext::channels::COMMANDS) {
            // The command/skill catalogue is assembled by the runtime's
            // slash-command registry; until that seam is wired the baseline is
            // an empty list rather than a missing channel.
            return serde_json::json!({ "commands": [] });
        }
        Value::Null
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

impl RuntimeBackend {
    /// Settle a tool call's confirmation from a client action.
    ///
    /// The settle key travels on the action's `_meta["x-manox"]["authId"]` —
    /// the same key the translator stamped when it surfaced the request, which
    /// is what the runtime's gate is registered under. An action that arrives
    /// without it (or for a call whose turn has already closed) settles
    /// nothing: a confirmation we cannot attribute must not guess an identity,
    /// because guessing would answer a *different* pending call.
    fn confirm_tool_call(
        &self,
        channel: &str,
        tool_call_id: &str,
        meta: Option<&ahp_types::common::JsonObject>,
        approved: bool,
    ) -> DispatchOutcome {
        let Some(session_id) = chat::id(channel) else {
            return DispatchOutcome::Rejected(format!("no runtime intent for {channel}"));
        };
        let Some(auth_id) = meta
            .and_then(|meta| meta.get(manox_ahp::ext::META_KEY))
            .and_then(|ext| ext.get("authId"))
            .and_then(Value::as_str)
        else {
            // The translator stamps every surfaced confirmation with its authId,
            // so a confirmation without one is a client that minted its own tool
            // call. Refusing keeps the gate's identity single-sourced.
            tracing::debug!(tool_call_id, "tool call confirmation without an authId");
            return DispatchOutcome::Ignored;
        };
        let response = if approved {
            manox_agent::permission::ToolAuthorizationResponse::Decision(
                manox_agent::permission::PermissionDecision::AllowOnce,
            )
        } else {
            manox_agent::permission::ToolAuthorizationResponse::Decision(
                manox_agent::permission::PermissionDecision::Deny,
            )
        };
        match self.server.ahp_inner().session_thread(session_id) {
            Some(thread) => {
                thread.with_mut(|t| t.respond_authorization(auth_id, response));
                DispatchOutcome::Accepted
            }
            None => DispatchOutcome::Rejected("unknown session".to_string()),
        }
    }

    /// Settle a question card from a client action (`chat/inputCompleted`).
    ///
    /// AHP's elicitation plane answers by request id, which is the same
    /// `authId` the translator surfaced the card under, so the two protocols
    /// name one identity.
    fn answer_question(&self, channel: &str, request_id: &str) -> DispatchOutcome {
        let Some(session_id) = chat::id(channel) else {
            return DispatchOutcome::Rejected(format!("no runtime intent for {channel}"));
        };
        match self.server.ahp_inner().session_thread(session_id) {
            Some(thread) => {
                // The answers themselves are not carried here: AHP's
                // `inputCompleted` is dispatched with the completed card's
                // answers already on the part, and the kernel reads them from
                // the same parked card. Completing the card is what the runtime
                // is asked for.
                thread.with_mut(|t| {
                    t.respond_question(
                        request_id,
                        manox_agent::questions::AskOutcome::Answered(Vec::new()),
                    )
                });
                DispatchOutcome::Accepted
            }
            None => DispatchOutcome::Rejected("unknown session".to_string()),
        }
    }

    /// Apply the model / effort / approval-mode / project selection a client
    /// merged into `SessionState.config`.
    ///
    /// AHP config is an open bag, so a client may send any subset. Each known
    /// key maps onto the runtime intent that owns it; an unknown key is not a
    /// refusal (the reducer already folded it) — the runtime simply has no work
    /// for it.
    fn apply_session_config(
        &self,
        channel: &str,
        config: &ahp_types::common::JsonObject,
    ) -> DispatchOutcome {
        let Some(session_id) = session::id(channel) else {
            return DispatchOutcome::Rejected(format!("no runtime intent for {channel}"));
        };
        let inner = self.server.ahp_inner();
        if let Some(model) = config.get("model").and_then(Value::as_str) {
            inner.set_model(session_id, model);
        }
        if let Some(effort) = config.get("reasoningEffort").and_then(Value::as_str) {
            inner.set_reasoning_effort(session_id, effort);
        }
        if let Some(mode) = config.get("approvalMode").and_then(Value::as_str) {
            inner.set_approval_mode(session_id, mode);
        }
        DispatchOutcome::Accepted
    }

    /// One page of a chat's turns, walking backwards from `cursor` (absent =
    /// the tail). The fold is the source, so a page is always journal truth.
    fn fetch_turns_window(
        &self,
        chat_id: &str,
        cursor: Option<&str>,
        limit: Option<i64>,
    ) -> Result<(Vec<Turn>, Option<String>), HostError> {
        const DEFAULT_PAGE: usize = 20;
        const MAX_PAGE: usize = 200;
        let Some(seeded) = self.seeds.lock().get(chat_id).cloned() else {
            return Ok((Vec::new(), None));
        };
        let end = match cursor {
            Some(cursor) => chat::index_of(cursor)
                .ok_or_else(|| HostError::InvalidParams("unrecognised turns cursor".into()))?,
            None => seeded.chat.turns.len(),
        };
        let page = limit
            .map(|limit| limit.clamp(1, MAX_PAGE as i64) as usize)
            .unwrap_or(DEFAULT_PAGE);
        let start = end.saturating_sub(page);
        let turns = seeded.chat.turns[start..end.min(seeded.chat.turns.len())].to_vec();
        let next = (start > 0).then(|| chat::cursor_of(start));
        Ok((turns, next))
    }
}

/// The live facts a store row does not carry, when this host has already
/// folded the session.
///
/// `status`, `activity` and the granted directories live only in the fold. When
/// a session has been seeded (a subscriber or a `createSession` folded it) the
/// real values are used; when it has not, the list answers what the store knows
/// rather than folding a journal to fill three fields — and omits what it
/// cannot know instead of inventing it.
#[derive(Clone)]
struct SeededFacts {
    status: u32,
    activity: Option<String>,
    directories: Option<Vec<String>>,
    title: String,
}

/// One `SessionSummary` from a store row plus the live facts, if any.
///
/// The row's [`manox_agent::db::ThreadSummary::display_title`] already applied
/// user-rename precedence, so it is the title of record; a seeded fold only
/// overrides it when it actually carries one.
fn summary_from_row(
    row: &manox_agent::db::ThreadSummary,
    store: &manox_agent::thread_store::ThreadStore,
    seeded: Option<SeededFacts>,
) -> SessionSummary {
    let facts_title = seeded.as_ref().map(|facts| facts.title.clone());
    let (status, activity, directories) = match seeded {
        Some(facts) => (facts.status, facts.activity, facts.directories),
        // A session the store knows and this host has not folded: idle unless a
        // turn is live in this process.
        None => (
            if store.is_running(&row.id) {
                ahp_types::state::SessionStatus::InProgress.bits()
            } else {
                ahp_types::state::SessionStatus::Idle.bits()
            },
            None,
            None,
        ),
    };
    SessionSummary {
        provider: row.provider_id.clone().unwrap_or_default(),
        // The row's `display_title` already applied user-rename precedence; a
        // seeded fold only wins when it actually carries a title.
        title: match &facts_title {
            Some(title) if !title.is_empty() => title.clone(),
            _ => row.display_title().to_string(),
        },
        status,
        activity,
        origin: None,
        project: (!row.project.is_empty()).then(|| ahp_types::state::ProjectInfo {
            uri: row.project.clone(),
            display_name: row.project.clone(),
        }),
        working_directories: directories,
        annotations: None,
        resource: session::uri(&row.id),
        created_at: String::new(),
        modified_at: String::new(),
        changes: None,
        meta: None,
    }
}

impl Backend for RuntimeBackend {
    fn root_state(&self) -> RootState {
        root::with_agents(self.agents(), None)
    }

    fn list_sessions(&self) -> Vec<SessionSummary> {
        // A list is built from the **store rows**, never from a per-session
        // journal fold. A fold reads a whole transcript, and this machine holds
        // hundreds of sessions totalling gigabytes, so folding each one to
        // produce a one-line summary made `listSessions` time out — and it is
        // both the first call a client makes and the source of every later
        // page. The v2 list path (`threads_snapshot`) reads the same rows for
        // the same reason.
        let Some(store) = manox_agent::thread_store::try_global() else {
            return Vec::new();
        };
        store.read(|state| {
            state
                .summaries()
                .iter()
                // A superseded predecessor is not the conversation's identity;
                // its successor row is (the v2 list rules the same way).
                .filter(|row| row.superseded_by.is_none())
                .map(|row| summary_from_row(row, state, self.seeded_status(&row.id)))
                .collect()
        })
    }

    fn session_summary(&self, session_id: &str) -> Option<SessionSummary> {
        let store = manox_agent::thread_store::try_global()?;
        let seeded = self.seeded_status(session_id);
        store.read(|state| {
            state
                .summary_by_id(session_id)
                .map(|row| summary_from_row(row, state, seeded.clone()))
        })
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

    #[cfg(feature = "terminal")]
    fn terminal_state(&self, terminal_id: &str) -> Option<TerminalState> {
        self.server.ahp_inner().ahp_terminal_state(terminal_id)
    }

    #[cfg(feature = "terminal")]
    fn create_terminal(
        &self,
        session_id: &str,
        terminal_id: &str,
        cols: u16,
        rows: u16,
    ) -> Result<(), HostError> {
        self.server
            .ahp_inner()
            .attach_terminal(session_id, cols, rows, Some(terminal_id.to_string()))
            .map(|_| ())
            .map_err(|error| HostError::Backend(error.message))
    }

    #[cfg(feature = "terminal")]
    fn dispose_terminal(&self, terminal_id: &str) -> Result<(), HostError> {
        self.server
            .ahp_inner()
            .dispose_terminal(terminal_id)
            .then_some(())
            .ok_or_else(|| HostError::NotFound(manox_ahp::channels::terminal::uri(terminal_id)))
    }

    #[cfg(not(feature = "terminal"))]
    fn terminal_state(&self, _terminal_id: &str) -> Option<TerminalState> {
        // The build has no terminal plane; an absent state answers `not found`,
        // never a fabricated one.
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
        .map_err(|error| HostError::Backend(error.message))?;
        // Seed and bridge the new session now, so a client that subscribes after
        // creating it gets a snapshot and then live actions.
        let _ = block_on(self.seeded(session_id));
        Ok(())
    }

    fn dispose_session(&self, session_id: &str) -> Result<(), HostError> {
        self.server.ahp_inner().dispose_session("ahp", session_id);
        Ok(())
    }

    fn create_chat(
        &self,
        session_id: &str,
        chat_id: &str,
        params: &CreateChatParams,
    ) -> Result<(), HostError> {
        // A manox chat *is* a journal, so a second chat in one session is a
        // branch: AHP's `createChat{source: fork}` maps onto the runtime's fork
        // intent, with the client-chosen chat id as the fork's target id.
        let Some(source) = params.source.as_ref() else {
            return Err(HostError::Unimplemented(
                "createChat without a source (a session holds one journal)".to_string(),
            ));
        };
        let (source_chat, turn_id) = match source {
            ahp_types::commands::ChatSource::Fork(fork) => (&fork.chat, &fork.turn_id),
            // A side chat keeps its source *out* of the visible history, which
            // is a context-injection policy the journal has no row for. Refuse
            // rather than fork a chat that would show the copied turns a side
            // chat must not show.
            ahp_types::commands::ChatSource::SideChat(_) => {
                return Err(HostError::Unimplemented("createChat{sideChat}".to_string()));
            }
            ahp_types::commands::ChatSource::Unknown(kind) => {
                return Err(HostError::Unimplemented(format!("createChat{{{kind}}}")));
            }
        };
        // A fork copies through a *completed* turn, and the AHP turn id is the
        // journal entry id with the translator's `t-` prefix (`translate/actions.rs`),
        // so the prefix is stripped back off to name the journal row.
        let Some(through_entry_id) = turn_id.strip_prefix("t-") else {
            return Err(HostError::InvalidParams(
                "a fork source must name a turn of this session".to_string(),
            ));
        };
        let Some(source_session_id) = chat::id(source_chat.as_str()) else {
            return Err(HostError::InvalidParams(
                "a fork source must be an ahp-chat URI".to_string(),
            ));
        };
        let owner = params
            .meta
            .as_ref()
            .and_then(|meta| meta.get("x-manox"))
            .and_then(|ext| ext.get("clientId"))
            .and_then(Value::as_str)
            .unwrap_or("ahp")
            .to_string();
        let inner = Arc::clone(self.server.ahp_inner());
        let intent = crate::agent_server::ForkIntent {
            source_session_id: source_session_id.to_string(),
            through_entry_id: through_entry_id.to_string(),
            target_session_id: Some(chat_id.to_string()),
            cwd: None,
            project: None,
            initial_model: None,
            approval_mode: None,
            reasoning_effort: None,
        };
        block_on(async move { crate::agent_server::fork_session(&inner, &owner, intent).await })
            .map_err(|error| HostError::Backend(error.message))?;
        let _ = session_id;
        // The forked journal is new to the host: seed and bridge it so a client
        // that subscribes right after creating it gets a snapshot and then live
        // actions.
        let _ = block_on(self.seeded(chat_id));
        Ok(())
    }

    fn dispose_chat(&self, chat_id: &str) -> Result<(), HostError> {
        // Disposing a chat retires its journal's live resources. The session
        // itself survives, so this disposes the engine rather than the row.
        self.server.ahp_inner().dispose_session("ahp", chat_id);
        Ok(())
    }

    fn fetch_turns(
        &self,
        chat_id: &str,
        cursor: Option<&str>,
        limit: Option<i64>,
    ) -> Result<Vec<Turn>, HostError> {
        Ok(self.fetch_turns_window(chat_id, cursor, limit)?.0)
    }

    fn fetch_turns_page(
        &self,
        chat_id: &str,
        cursor: Option<&str>,
        limit: Option<i64>,
    ) -> Result<(Vec<Turn>, Option<String>), HostError> {
        self.fetch_turns_window(chat_id, cursor, limit)
    }

    fn dispatch(
        &self,
        channel: &str,
        action: &StateAction,
        origin: &ActionOrigin,
    ) -> DispatchOutcome {
        match action {
            // ── chat actions ───────────────────────────────────────────────
            StateAction::ChatTurnStarted(started) => {
                let Some(session_id) = chat::id(channel) else {
                    return DispatchOutcome::Rejected(format!("no runtime intent for {channel}"));
                };
                let text = started.message.text.clone();
                let owner = origin.client_id.clone();
                let inner = Arc::clone(self.server.ahp_inner());
                let target = session_id.to_string();
                let session_id = target.clone();
                match block_on(async move {
                    inner
                        .submit(&owner, &target, text, Vec::new(), None, None)
                        .await
                }) {
                    Ok(_) => {
                        // A submit materializes the engine, so a session that was
                        // cold when the client subscribed has no bridge yet.
                        let _ = block_on(self.seeded(&session_id));
                        DispatchOutcome::Accepted
                    }
                    Err(error) => DispatchOutcome::Rejected(error.message),
                }
            }
            StateAction::ChatPendingMessageSet(set) => {
                let Some(session_id) = chat::id(channel) else {
                    return DispatchOutcome::Rejected(format!("no runtime intent for {channel}"));
                };
                // A steering message is injected into the *running* turn; the
                // runtime's steer path both parks it (when idle) and injects it
                // (when running), so one intent covers both kinds. The pending
                // message id is the steer id — the identity the journal row and
                // the echo retirement share.
                let text = set.message.text.clone();
                let inner = Arc::clone(self.server.ahp_inner());
                let target = session_id.to_string();
                match inner.steer(&target, set.id.clone(), text, Vec::new(), None) {
                    Ok(_) => DispatchOutcome::Accepted,
                    Err(error) => DispatchOutcome::Rejected(error.message),
                }
            }
            StateAction::ChatPendingMessageRemoved(removed) => {
                let Some(session_id) = chat::id(channel) else {
                    return DispatchOutcome::Rejected(format!("no runtime intent for {channel}"));
                };
                // Both kinds name a parked follow-up the runtime holds by id, so
                // withdrawing one is the same intent either way.
                self.server
                    .ahp_inner()
                    .drop_queued(session_id, removed.id.clone());
                DispatchOutcome::Accepted
            }
            StateAction::ChatTurnCancelled(_) => {
                let Some(session_id) = chat::id(channel) else {
                    return DispatchOutcome::Rejected(format!("no runtime intent for {channel}"));
                };
                match self.server.ahp_inner().session_thread(session_id) {
                    Some(thread) => {
                        thread.with_mut(|t| t.cancel());
                        DispatchOutcome::Accepted
                    }
                    None => DispatchOutcome::Rejected("unknown session".to_string()),
                }
            }
            StateAction::ChatToolCallConfirmed(confirmed) => self.confirm_tool_call(
                channel,
                &confirmed.tool_call_id,
                confirmed.meta.as_ref(),
                confirmed.approved,
            ),
            StateAction::ChatInputCompleted(completed) => {
                self.answer_question(channel, &completed.request_id)
            }
            // ── session actions ────────────────────────────────────────────
            StateAction::SessionConfigChanged(changed) => {
                self.apply_session_config(channel, &changed.config)
            }
            StateAction::SessionWorkingDirectorySet(set) => {
                let Some(session_id) = session::id(channel) else {
                    return DispatchOutcome::Rejected(format!("no runtime intent for {channel}"));
                };
                let Some(path) = file_uri_to_path(set.directory.as_str()) else {
                    return DispatchOutcome::Rejected(
                        "working directories must be file:// URIs".to_string(),
                    );
                };
                let inner = Arc::clone(self.server.ahp_inner());
                let target = session_id.to_string();
                block_on(async move { inner.set_cwd(&target, &path).await });
                DispatchOutcome::Accepted
            }
            StateAction::SessionTitleChanged(changed) => {
                // A user rename is real work: the runtime journals the title
                // entry and writes the sidecar, so the name outlives the
                // connection and outranks a model-generated one. A blank title
                // is refused rather than echoed — the reducer would fold an
                // empty name while the session kept its old one, which is
                // exactly the silent divergence the echo is supposed to avoid.
                let Some(session_id) = session::id(channel) else {
                    return DispatchOutcome::Rejected(format!("no runtime intent for {channel}"));
                };
                use manox_agent::thread_store::RenameOutcome;
                match self
                    .server
                    .ahp_inner()
                    .rename_thread(session_id, &changed.title)
                {
                    RenameOutcome::Renamed => DispatchOutcome::Accepted,
                    RenameOutcome::Blank => {
                        DispatchOutcome::Rejected("a session title cannot be blank".to_string())
                    }
                    RenameOutcome::UnknownSession => {
                        DispatchOutcome::Rejected(format!("unknown session: {session_id}"))
                    }
                    // Not durable (no engine and no journal file, or another
                    // process drives the session), so it must not be reported as
                    // accepted: every subscriber would then fold a title no
                    // reader can ever replay.
                    RenameOutcome::NotPersisted => DispatchOutcome::Rejected(
                        "the session's journal is not writable from this process".to_string(),
                    ),
                }
            }
            StateAction::SessionIsArchivedChanged(changed) => {
                let Some(session_id) = session::id(channel) else {
                    return DispatchOutcome::Rejected(format!("no runtime intent for {channel}"));
                };
                self.server.ahp_inner().archive_thread(
                    &origin.client_id,
                    session_id,
                    changed.is_archived,
                );
                DispatchOutcome::Accepted
            }
            // Actions the acceptance table admits and the reducer folds, but
            // that describe client-side state the runtime does not own (draft
            // text, read flags, turn resumption, result confirmation). They are
            // echoed as accepted so every subscriber observes the same sequence,
            // and no runtime work follows.
            StateAction::ChatDraftChanged(_)
            | StateAction::ChatTurnResume(_)
            | StateAction::ChatToolCallResultConfirmed(_)
            | StateAction::ChatInputAnswerChanged(_)
            | StateAction::ChatQueuedMessagesReordered(_)
            | StateAction::SessionIsReadChanged(_)
            | StateAction::SessionActiveClientRemoved(_) => DispatchOutcome::Ignored,
            other => DispatchOutcome::Rejected(format!(
                "no runtime intent yet: {}",
                manox_ahp::wire::action_tag(other)
            )),
        }
    }

    fn resources(&self) -> Option<&dyn ResourcePlane> {
        // The plane is built once, over the roots the runtime was started
        // with; per-session grants are a refinement the fence does not have a
        // seam for yet, so the base cwd is the root.
        Some(&self.resources)
    }

    /// The baseline a subscriber to a declared extension channel receives.
    ///
    /// Extension channels carry no AHP-reducible state, so `subscribe` has no
    /// snapshot to hand back; without this the six channels advertised in
    /// `_meta["x-manox"]` would accept a subscription and then say nothing —
    /// the declaration would be a promise the host does not keep. The baseline
    /// is the journal's own fold of that channel's actions, so a late subscriber
    /// converges on the same state a live one reached by following along.
    ///
    /// A channel with no folded state still answers: an empty object is "this
    /// channel is served and currently has nothing", which is a different and
    /// actionable statement from silence.
    fn extension_baseline(&self, channel: &str) -> Option<(String, Value)> {
        if !manox_ahp::ext::is_extension_channel(channel) {
            return None;
        }
        // The per-session channels fold one session's journal; the catalogue
        // channels (`x-manox-workspaces://`, `x-manox-commands://`) describe the
        // host and carry no session id.
        let state = if manox_ahp::ext::is_session_scoped_channel(channel) {
            let session_id = channel
                .split_once(":/")?
                .1
                .split('/')
                .next()
                .filter(|id| !id.is_empty())?;
            match block_on(self.seeded(session_id)) {
                Some(seeded) => seeded
                    .extensions
                    .get(channel)
                    .map(|state| serde_json::to_value(state).unwrap_or(Value::Null))
                    .unwrap_or(Value::Null),
                // An unknown session is not a served channel: answering an
                // empty baseline would claim state for a session that has none.
                None => return None,
            }
        } else {
            self.catalogue_baseline(channel)
        };
        Some((
            manox_ahp::ext::BASELINE_NOTIFICATION.to_string(),
            serde_json::json!({ "channel": channel, "state": state }),
        ))
    }

    fn extension(&self, method: &str, params: &Value) -> Result<Value, HostError> {
        // The extension surface is declared once in `ext::commands`; this maps
        // the subset the runtime actually performs onto its existing intents.
        // Everything else answers `Unimplemented`, which is the honest reply
        // for a name we advertise but do not serve.
        match method {
            manox_ahp::ext::commands::COMPACT => {
                let session_id = extension_session(params)?;
                let instructions = params
                    .get("instructions")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                match self.server.ahp_inner().session_thread(&session_id) {
                    Some(_) => {
                        self.server.ahp_inner().compact(&session_id, instructions);
                        Ok(Value::Null)
                    }
                    None => Err(HostError::SessionNotFound(session_id)),
                }
            }
            manox_ahp::ext::commands::PLAN_EXECUTE => {
                let session_id = extension_session(params)?;
                let Some(plan_file) = params.get("planFile").and_then(Value::as_str) else {
                    return Err(HostError::InvalidParams(
                        "x-manox/planExecute needs planFile".to_string(),
                    ));
                };
                match self.server.ahp_inner().session_thread(&session_id) {
                    Some(_) => {
                        self.server.ahp_inner().plan_seed(&session_id, plan_file);
                        Ok(Value::Null)
                    }
                    None => Err(HostError::SessionNotFound(session_id)),
                }
            }
            // A client asked for a surface this build declares but does not
            // perform. `-32080` tells it apart from "you sent a bad request".
            other => Err(HostError::Unimplemented(other.to_string())),
        }
    }
}

/// The session an extension command names, from its `channel`.
///
/// Every extension command carries `channel`, and the ones the runtime
/// performs are session-scoped, so the channel must be a session URI.
fn extension_session(params: &Value) -> Result<String, HostError> {
    let Some(channel) = params.get("channel").and_then(Value::as_str) else {
        return Err(HostError::InvalidParams(
            "an extension command needs a channel".to_string(),
        ));
    };
    manox_ahp::channels::session::id(channel)
        .map(str::to_string)
        .ok_or_else(|| HostError::InvalidParams(format!("{channel} is not a session channel")))
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
