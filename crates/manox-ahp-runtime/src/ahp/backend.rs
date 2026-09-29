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

use ahp_types::actions::{ActionOrigin, ChatPendingMessageRemovedAction, StateAction};
use ahp_types::commands::{CreateChatParams, CreateSessionParams};
use ahp_types::state::{
    AgentInfo, ChatState, RootState, SessionModelInfo, SessionState, SessionSummary, TerminalState,
    Turn,
};
use manox_ahp::backend::{Backend, DispatchOutcome};
#[cfg(feature = "terminal")]
use manox_ahp::channels::terminal;
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
use crate::runtime_trait::SessionRuntime;

/// Forward one gated MCP method to the upstream server over the registry's
/// rmcp client. Params decode straight into the rmcp request types (MCP wire
/// shapes) and the typed result serializes back — the proxy adds nothing.
#[cfg(feature = "mcp")]
async fn proxy_mcp_method(
    peer: &rmcp::service::Peer<rmcp::service::RoleClient>,
    method: &str,
    params: &Value,
) -> Result<Value, HostError> {
    use rmcp::model as mcp_model;
    let invalid = |err: serde_json::Error| HostError::InvalidParams(err.to_string());
    let upstream = |err: rmcp::service::ServiceError| HostError::Backend(err.to_string());
    let encode = |err: serde_json::Error| HostError::Backend(err.to_string());
    // The proxy runs on a synchronous seam (`block_in_place`); rmcp's
    // convenience methods carry no default timeout, so a stuck upstream
    // would park the worker and hang the client's request forever. Every
    // forwarded call rides the same explicit deadline.
    let deadline = std::time::Duration::from_secs(30);
    tokio::time::timeout(deadline, async {
        match method {
            "tools/list" => {
                let p: Option<mcp_model::PaginatedRequestParams> =
                    serde_json::from_value(params.clone()).map_err(invalid)?;
                let result = peer.list_tools(p).await.map_err(upstream)?;
                serde_json::to_value(result).map_err(encode)
            }
            "tools/call" => {
                let p: mcp_model::CallToolRequestParams =
                    serde_json::from_value(params.clone()).map_err(invalid)?;
                let result = peer.call_tool(p).await.map_err(upstream)?;
                serde_json::to_value(result).map_err(encode)
            }
            "resources/list" => {
                let p: Option<mcp_model::PaginatedRequestParams> =
                    serde_json::from_value(params.clone()).map_err(invalid)?;
                let result = peer.list_resources(p).await.map_err(upstream)?;
                serde_json::to_value(result).map_err(encode)
            }
            "resources/templates/list" => {
                let p: Option<mcp_model::PaginatedRequestParams> =
                    serde_json::from_value(params.clone()).map_err(invalid)?;
                let result = peer.list_resource_templates(p).await.map_err(upstream)?;
                serde_json::to_value(result).map_err(encode)
            }
            "resources/read" => {
                let p: mcp_model::ReadResourceRequestParams =
                    serde_json::from_value(params.clone()).map_err(invalid)?;
                let result = peer.read_resource(p).await.map_err(upstream)?;
                serde_json::to_value(result).map_err(encode)
            }
            other => Err(HostError::MethodNotFound(format!(
                "{other} is not in the served mcp:// capability set"
            ))),
        }
    })
    .await
    .map_err(|_| {
        HostError::Backend(format!(
            "mcp upstream timed out after {}s: {method}",
            deadline.as_secs()
        ))
    })?
}

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
pub struct RuntimeBackend {
    server: Arc<dyn SessionRuntime>,
    cwd: PathBuf,
    /// Set once by the runtime right after `Host::new`: the bridge needs the host
    /// to publish, and the host needs the backend to seed — a cycle broken by
    /// handing the weak reference over after both exist.
    host: OnceLock<Weak<manox_ahp::Host>>,
    seeds: Mutex<HashMap<String, Arc<Seeded>>>,
    bridges: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
    /// The plan file each open review card names, keyed by its request id. The
    /// card's `chat/inputCompleted` carries only the request id and the verdict,
    /// so the approve path recovers the file from here.
    plan_reviews: Mutex<HashMap<String, String>>,
    /// The bridge task needs an `Arc` of this backend while the host holds only
    /// `&self` through the trait, so the backend keeps a weak handle to itself.
    me: OnceLock<Weak<RuntimeBackend>>,
    /// The `resource*` file plane, fenced to the runtime's working directory.
    resources: super::resources::RuntimeResources,
    /// Live-output pumps by terminal id (see [`Self::ensure_terminal_pump`]).
    #[cfg(feature = "terminal")]
    terminal_pumps: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
}

impl RuntimeBackend {
    pub fn new(server: Arc<dyn SessionRuntime>, cwd: PathBuf) -> Arc<Self> {
        let resources = super::resources::RuntimeResources::new(vec![cwd.clone()]);
        let backend = Arc::new(Self {
            server,
            cwd,
            host: OnceLock::new(),
            seeds: Mutex::new(HashMap::new()),
            bridges: Mutex::new(HashMap::new()),
            plan_reviews: Mutex::new(HashMap::new()),
            me: OnceLock::new(),
            resources,
            #[cfg(feature = "terminal")]
            terminal_pumps: Mutex::new(HashMap::new()),
        });
        let _ = backend.me.set(Arc::downgrade(&backend));
        backend
    }

    fn me(&self) -> Option<Arc<Self>> {
        self.me.get().and_then(Weak::upgrade)
    }

    pub fn attach_host(&self, host: &Arc<manox_ahp::Host>) {
        let _ = self.host.set(Arc::downgrade(host));
        #[cfg(feature = "mcp")]
        self.spawn_mcp_pump(host);
    }

    /// Republish MCP registry transitions as `session/mcpServerStateChanged`
    /// and the resulting `session/serverToolsChanged` on every seeded session
    /// channel.
    ///
    /// Publishing is unconditional on subscribers because the publish *is*
    /// the host store's update: a session that sits seeded-but-unwatched
    /// must still fold the transition, or its next subscriber is handed a
    /// stale snapshot. A session the host has never seeded is skipped — its
    /// first seed overlays the registry snapshot directly, and folding into
    /// a state that does not exist yet would only be reducer noise.
    #[cfg(feature = "mcp")]
    fn spawn_mcp_pump(&self, host: &Arc<manox_ahp::Host>) {
        let mut events = manox_agent::mcp::subscribe_events();
        let host = Arc::clone(host);
        let me = self.me();
        manox_agent::runtime::handle().spawn(async move {
            loop {
                match events.recv().await {
                    Ok(event) => {
                        let state_action =
                            ahp_types::actions::StateAction::SessionMcpServerStateChanged(
                                Box::new(super::mcp::state_changed(event)),
                            );
                        // Every transition moves the inventory: ready adds a
                        // server's tools, error/stop removes them. Full
                        // replacement, straight from the registry snapshot.
                        let tools_action =
                            ahp_types::actions::StateAction::SessionServerToolsChanged(
                                ahp_types::actions::SessionServerToolsChangedAction {
                                    tools: super::mcp::server_tools(),
                                },
                            );
                        // A toggle that enables a previously filtered-out
                        // server *creates* a slot, which no narrower action
                        // expresses: the full-replacement catalogue covers
                        // appearance and disappearance alike.
                        let catalogue_action =
                            ahp_types::actions::StateAction::SessionCustomizationsChanged(
                                ahp_types::actions::SessionCustomizationsChangedAction {
                                    customizations: super::mcp::customizations()
                                        .into_iter()
                                        .map(|server| {
                                            ahp_types::state::Customization::McpServer(Box::new(
                                                server,
                                            ))
                                        })
                                        .collect(),
                                },
                            );
                        let Some(backend) = me.as_ref() else {
                            continue;
                        };
                        // Collect the ids before looping: the `for` head's
                        // temporary would otherwise hold the seeds read lock
                        // across every publish (which takes the store write
                        // lock) — a new lock-holding-across-work pattern with
                        // no benefit.
                        let session_ids: Vec<String> =
                            backend.seeds.lock().keys().cloned().collect();
                        for session_id in session_ids {
                            if host.has_session(&session_id) {
                                let channel = manox_ahp::channels::session::uri(&session_id);
                                host.publish(&channel, state_action.clone(), None);
                                host.publish(&channel, tools_action.clone(), None);
                                host.publish(&channel, catalogue_action.clone(), None);
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        // Missed transitions are corrected by the next seed
                        // (the overlay always wins); the lag is worth a line.
                        tracing::warn!("MCP event pump lagged, {n} transitions replayed from seed");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    fn host(&self) -> Option<Arc<manox_ahp::Host>> {
        self.host.get().and_then(Weak::upgrade)
    }

    /// `root/terminalsChanged` with the full replacement catalogue, taken
    /// from the runtime's metadata-only seam. A no-op until a host is
    /// attached (a terminal spawned before the host exists is picked up by
    /// the next root seed instead).
    fn publish_terminals_changed(&self) {
        let Some(host) = self.host() else {
            return;
        };
        host.publish(
            manox_ahp::channels::root::URI,
            StateAction::RootTerminalsChanged(ahp_types::actions::RootTerminalsChangedAction {
                terminals: self.server.terminal_infos(),
            }),
            None,
        );
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
        // The host's live chat state is the client-visible mirror and wins
        // over a fresh fold: the fold re-mints turn ids under fresh synthetic
        // names that the connected clients — and the bridge resuming against
        // them — have never seen. The fold stays the authority only where
        // the host has no state yet (a cold session, a fresh process); a
        // snapshot answered from the host state is exactly what the live
        // stream continues from, so late subscribers and the bridge agree.
        let chat = match self.host().and_then(|host| host.chat_state(session_id)) {
            Some(live) if live.active_turn.is_some() || !live.turns.is_empty() => live,
            _ => chat,
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

    /// Start the live bridge for `session_id` (idempotent; restarts a bridge
    /// that already exited).
    fn ensure_bridge(&self, session_id: &str) {
        let mut bridges = self.bridges.lock();
        if let Some(task) = bridges.get(session_id)
            && !task.is_finished()
        {
            return;
        }
        bridges.remove(session_id);
        let Some(thread) = self.server.journal_feed(session_id) else {
            // A cold session (no live engine) has no feed to bridge; its state
            // still answers from the journal, and a submit materializes the
            // engine, which re-runs this path.
            tracing::debug!(session = %session_id, "bridge: no live feed yet (cold session)");
            return;
        };
        let Some(host) = self.host() else {
            return;
        };
        let Some(backend) = self.me() else {
            return;
        };
        tracing::info!(session = %session_id, "bridge: starting");
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
        let mut first_event = true;
        tracing::info!(session = %session_id, tail, "bridge: subscribed to the journal feed");
        loop {
            match feed.recv().await {
                Ok(manox_agent::engine::JournalFeed::Event(event)) => {
                    if first_event {
                        first_event = false;
                        tracing::info!(
                            session = %session_id,
                            seq = event.seq,
                            "bridge: first journal event"
                        );
                    }
                    if event.seq <= tail {
                        continue;
                    }
                    tail = event.seq;
                    let Some(entry) = crate::translate::wire_entry(event.seq, &event.entry) else {
                        continue;
                    };
                    // A finished turn is when the agent's file changes settle:
                    // recompute the changeset so subscribers watch one stream
                    // instead of polling. Detached — the scan is synchronous
                    // and cheap, but it must not stall the journal pump.
                    if matches!(&entry.event, JournalWireEvent::TurnFinish { .. })
                        && let Some(backend) = self.me()
                    {
                        let session = session_id.clone();
                        manox_agent::runtime::handle().spawn_blocking(move || {
                            for (channel, action) in
                                super::changeset::Engine::global().recompute(&session)
                            {
                                // Only channels the host has ensured (a
                                // subscriber or a dispatch landed): the
                                // publish *is* the store update, and folding
                                // into a state that does not exist would
                                // just log OutOfScope noise each turn.
                                if let Some(host) = backend.host()
                                    && host.has_changeset(&channel)
                                {
                                    host.publish(&channel, action, None);
                                }
                            }
                        });
                    }
                    // The client's own `turnStarted` dispatch already expressed the
                    // user row and the turn boundary: re-folding them would publish a
                    // second `pendingMessageSet` and a second `turnStarted`, and the
                    // reducer overwrites `active_turn` unconditionally, orphaning the
                    // streamed parts. Skip both; the parts that follow address the
                    // client's turn id.
                    if matches!(&entry.event, JournalWireEvent::Message { role, .. } if role == "user")
                        || matches!(&entry.event, JournalWireEvent::TurnStart)
                    {
                        continue;
                    }
                    // Track the plan file behind each review card so the approve
                    // path (which arrives as a bare `chat/inputCompleted`) can
                    // recover it without re-reading the thread.
                    if let JournalWireEvent::PlanReview {
                        state, plan_file, ..
                    } = &entry.event
                    {
                        let request_id = manox_ahp::translate::plan_review_request_id(&entry.id);
                        if state == "resolved" {
                            self.plan_reviews.lock().remove(&request_id);
                        } else if let Some(plan_file) = plan_file {
                            self.plan_reviews
                                .lock()
                                .insert(request_id, plan_file.clone());
                        }
                    }
                    // Resume the turn bookkeeping against the active turn the client
                    // opened, so streamed parts carry its id (and never publish a
                    // synthetic `turnStarted` that would replace it).
                    if let Some(state) = host.chat_state(&session_id)
                        && let Some(active) = &state.active_turn
                    {
                        translator.open_with_id(active.id.clone(), active.started_at.clone());
                    }
                    let thread_id = self
                        .seeds
                        .lock()
                        .get(&session_id)
                        .map(|seeded| seeded.thread_id.clone())
                        .unwrap_or_else(|| session_id.clone());
                    for emitted in translator.on_entry(&session_id, &thread_id, &entry) {
                        if matches!(&emitted.action, StateAction::ChatInputRequested(_)) {
                            tracing::info!(
                                session = %session_id,
                                seq = entry.seq,
                                "bridge: publishing chat/inputRequested"
                            );
                        }
                        host.publish(&emitted.channel, emitted.action, None);
                    }
                    // A landed assistant row changes the Q face: republish the
                    // aggregate so the metrics channel stays a read model of
                    // the journal, not a stream of per-call rows the client
                    // would have to re-aggregate (the v2 `GetConversationInfo`
                    // contract, now push).
                    if matches!(&entry.event, JournalWireEvent::Message { role, .. } if role == "assistant")
                        && let Some(metrics) = self.server.conversation_metrics(&session_id)
                    {
                        let channel = format!("{}{session_id}", manox_ahp::ext::channels::METRICS);
                        host.publish(
                            &channel,
                            StateAction::Unknown(serde_json::json!({
                                "type": manox_ahp::ext::actions::METRICS_CHANGED,
                                "kind": "conversation",
                                "data": metrics,
                            })),
                            None,
                        );
                    }
                }
                Ok(manox_agent::engine::JournalFeed::Lagged(_)) => {
                    translator = Translator::new();
                    if let Some(fold) = fold_journal(&session_id, &session_id).await {
                        let Some(session_state) = super::session_state(&session_id).await else {
                            continue;
                        };
                        tail = fold.tail;
                        host.seed_session(&session_id, session_state);
                        // The host's chat state stays as the client last saw it —
                        // deliberately NOT re-seeded from the fold. The fold
                        // re-mints the active turn under a fresh synthetic id, and
                        // the resumed bridge would adopt it (`open_with_id`)
                        // without announcing it: every streamed part from here on
                        // carries an id the client's active turn doesn't match, so
                        // its reducer drops them all while the stale turn keeps
                        // the UI "running" forever. Keeping the client-visible id
                        // lets the reset translator resume against the turn the
                        // client actually holds; the fold remains the durable
                        // authority (a fresh subscriber's snapshot recomputes it).
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        session = %session_id,
                        %error,
                        "bridge: journal feed closed, exiting"
                    );
                    break;
                }
            }
        }
    }

    /// Start the live-output pump for a terminal (idempotent).
    ///
    /// The terminal channel's output leg: raw PTY bytes arrive on the terminal's
    /// raw tap, are decoded to text, and go out as `terminal/data` actions
    /// through the host's single write path — so a subscriber receives output on
    /// the same numbered stream as every other channel state, and the browser of
    /// `serverSeq` stays intact.
    ///
    /// The v2 follow-terminal stream relays the same tap per *stream*; an AHP
    /// terminal is a channel, so one pump serves every subscriber.
    #[cfg(feature = "terminal")]
    fn ensure_terminal_pump(&self, terminal_id: &str) {
        if self.terminal_pumps.lock().contains_key(terminal_id) {
            return;
        }
        let Some(host) = self.host() else {
            return;
        };
        let Some(server) = self.server.terminal_raw_tap(terminal_id) else {
            return;
        };
        let mut raw_rx = server;
        let uri = manox_ahp::channels::terminal::uri(terminal_id);
        let id = terminal_id.to_string();
        let task = manox_agent::runtime::handle().spawn(async move {
            // PTY chunks are byte fragments, not character boundaries, so a
            // multi-byte character can straddle two chunks. Decoding each chunk
            // on its own would replace the split halves with U+FFFD and corrupt
            // exactly the non-ASCII output this runtime produces most of.
            let mut pending: Vec<u8> = Vec::new();
            while let Ok(chunk) = raw_rx.recv().await {
                pending.extend_from_slice(&chunk);
                let text = match std::str::from_utf8(&pending) {
                    Ok(text) => {
                        let owned = text.to_string();
                        pending.clear();
                        owned
                    }
                    Err(error) => {
                        let valid = error.valid_up_to();
                        if valid == 0 {
                            // Nothing decodable yet; wait for the rest.
                            continue;
                        }
                        let owned = String::from_utf8_lossy(&pending[..valid]).into_owned();
                        pending.drain(..valid);
                        owned
                    }
                };
                if text.is_empty() {
                    continue;
                }
                host.publish(
                    &uri,
                    ahp_types::actions::StateAction::TerminalData(
                        ahp_types::actions::TerminalDataAction { data: text },
                    ),
                    None,
                );
            }
            tracing::debug!(terminal = %id, "terminal output pump ended");
        });
        self.terminal_pumps
            .lock()
            .insert(terminal_id.to_string(), task);
    }

    /// Register (or replace) one active client's contributed tools.
    ///
    /// This is the AHP face of v2's `RegisterSessionTools`, and it fills the
    /// same store: the engine's `embedder_tools` provider is what makes a
    /// registered tool callable by the model, so routing the protocol action
    /// here — rather than into a parallel table — is what keeps one source of
    /// truth for "the tools this session's clients contribute".
    ///
    /// Full replacement per client, matching `session/activeClientSet`'s
    /// upsert-by-`clientId` semantics and the v2 call's own contract.
    ///
    /// Fail-closed on every input the runtime cannot honour: a session with no
    /// live engine would otherwise accept the registration, answer `Accepted`,
    /// fold the tools into the state every subscriber reads — and never mount
    /// them, so the model would be offered a tool that cannot run.
    fn register_client_tools(
        &self,
        session_id: &str,
        client: &ahp_types::state::SessionActiveClient,
    ) -> Result<(), String> {
        if client.client_id.is_empty() {
            return Err("an active client needs a clientId".to_string());
        }
        if !self.server.has_session(session_id) {
            return Err(format!(
                "unknown session: {session_id} (no live engine to register tools on)"
            ));
        }
        let mut specs = Vec::with_capacity(client.tools.len());
        for tool in &client.tools {
            specs.push(client_tool_spec(tool)?);
        }
        self.server
            .set_embedder_tools(session_id, &client.client_id, specs);
        Ok(())
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
            // The catalogue carries both halves of the workspace account: the
            // folder order and each partition's thread order. Manual ordering is
            // a user-visible feature with no AHP slot, so this channel is where a
            // client reads back what its `x-manox/orderChanged` moves produced.
            let (workspaces, order) = manox_agent::thread_store::try_global()
                .map(|store| {
                    store.read(|state| {
                        let (groups, accounts) = state.sidebar_order();
                        (groups.to_vec(), accounts.clone())
                    })
                })
                .unwrap_or_default();
            return serde_json::json!({ "workspaces": workspaces, "order": order });
        }
        if channel.starts_with(manox_ahp::ext::channels::COMMANDS) {
            return command_catalogue();
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
                let raw: Vec<manox_harness::types::Model> = registry
                    .models()
                    .into_iter()
                    .filter(|model| model.provider == provider)
                    .collect();
                // The human display name (metadata `provider_display_name`,
                // e.g. "Packy API") — one submenu per display name, so wire
                // variants of one provider merge with their own tags.
                let display_name = raw
                    .first()
                    .map(manox_agent::provider_glue::display_provider_name)
                    .unwrap_or_else(|| provider.clone());
                let models = raw
                    .into_iter()
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
                        // The wire api ("anthropic" / "openai_responses" /
                        // "openai_completions") is the discriminator clients
                        // tag model rows with; AHP has no native slot, so it
                        // rides the extension meta namespace.
                        meta: Some(
                            serde_json::json!({ "x-manox": { "api": model.api } })
                                .as_object()
                                .cloned()
                                .expect("a literal object"),
                        ),
                    })
                    .collect();
                AgentInfo {
                    provider: provider.clone(),
                    display_name,
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
        match self.server.confirm_tool_call(session_id, auth_id, approved) {
            Ok(()) => DispatchOutcome::Accepted,
            Err(error) => DispatchOutcome::Rejected(error.message),
        }
    }

    /// Settle a question card from a client action (`chat/inputCompleted`).
    ///
    /// AHP's elicitation plane answers by request id, which is the same
    /// `authId` the translator surfaced the card under, so the two protocols
    /// name one identity. The client's answers ride the completed card and are
    /// mapped here onto the kernel's `AskAnswer` list.
    fn answer_question(
        &self,
        channel: &str,
        request_id: &str,
        answers: Vec<manox_agent::permission::AskAnswer>,
    ) -> DispatchOutcome {
        let Some(session_id) = chat::id(channel) else {
            return DispatchOutcome::Rejected(format!("no runtime intent for {channel}"));
        };
        match self.server.answer_question(session_id, request_id, answers) {
            Ok(()) => DispatchOutcome::Accepted,
            Err(error) => DispatchOutcome::Rejected(error.message),
        }
    }

    /// Resolve a plan-review verdict (a `chat/inputCompleted` against a
    /// `plan-review:` card). Approve seeds plan execution; any other answer is
    /// the refine path — plan mode stays active and the user's next message
    /// carries the feedback.
    fn resolve_plan_review(
        &self,
        channel: &str,
        completed: &ahp_types::actions::ChatInputCompletedAction,
    ) -> DispatchOutcome {
        let Some(session_id) = chat::id(channel) else {
            return DispatchOutcome::Rejected(format!("no runtime intent for {channel}"));
        };
        let approved = map_answers(&completed.answers)
            .iter()
            .any(|a| a.selected.iter().any(|s| s == "approve"));
        if !approved {
            return DispatchOutcome::Accepted;
        }
        let plan_file = self.plan_reviews.lock().get(&completed.request_id).cloned();
        let Some(plan_file) = plan_file else {
            return DispatchOutcome::Rejected(
                "plan review answered without a known plan file".to_string(),
            );
        };
        match self.server.plan_seed(session_id, &plan_file) {
            Ok(()) => DispatchOutcome::Accepted,
            Err(error) => DispatchOutcome::Rejected(error.message),
        }
    }

    /// Apply the model / effort / approval-mode / project selection a client
    /// merged into `SessionState.config`.
    ///
    /// AHP config is an open bag, so a client may send any subset. Each known
    /// key maps onto the runtime intent that owns it; an unknown key is not a
    /// refusal (the reducer already folded it) — the runtime simply has no work
    /// for it.
    ///
    /// A runtime refusal on a key the reducer DID fold is a rejection, not a
    /// note: answering `Accepted` would leave the client's state on a model or
    /// mode the session never adopted.
    fn apply_session_config(
        &self,
        channel: &str,
        config: &ahp_types::common::JsonObject,
    ) -> DispatchOutcome {
        let Some(session_id) = session::id(channel) else {
            return DispatchOutcome::Rejected(format!("no runtime intent for {channel}"));
        };
        let applied = [
            config
                .get("model")
                .and_then(Value::as_str)
                .map(|model| self.server.set_model(session_id, model)),
            config
                .get("reasoningEffort")
                .and_then(Value::as_str)
                .map(|effort| self.server.set_reasoning_effort(session_id, effort)),
            config
                .get("approvalMode")
                .and_then(Value::as_str)
                .map(|mode| self.server.set_approval_mode(session_id, mode)),
        ];
        for outcome in applied.into_iter().flatten() {
            if let Err(error) = outcome {
                return DispatchOutcome::Rejected(error.message);
            }
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

/// The command/skill catalogue served on `x-manox-commands://`.
///
/// AHP has no notion of a slash command, so this channel is where a client
/// reads the palette it can offer. The three sources are the same ones the v2
/// `ListCommands` call read, in the same precedence: built-ins first, then
/// user commands, then skills, with a name claimed by an earlier source
/// winning — a user command that shadows a built-in must not appear twice.
fn command_catalogue() -> Value {
    let mut commands: Vec<Value> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for meta in manox_agent::slash_builtins::BUILTIN_SLASH_COMMANDS {
        seen.insert(meta.name.to_string());
        commands.push(serde_json::json!({
            "name": meta.name,
            "description": meta.description,
            "kind": "command",
            "argumentHint": Value::Null,
        }));
    }
    if let Some(registry) = manox_agent::command::try_global() {
        for (key, def) in registry.entries() {
            if !seen.insert(key.clone()) {
                continue;
            }
            commands.push(serde_json::json!({
                "name": key,
                "description": def.description,
                "kind": "command",
                "argumentHint": def.argument_hint,
            }));
        }
    }
    if let Some(registry) = manox_agent::skill::try_global() {
        for (key, def) in registry.entries() {
            if !seen.insert(key.clone()) {
                continue;
            }
            commands.push(serde_json::json!({
                "name": key,
                "description": def.description,
                "kind": "skill",
                "argumentHint": Value::Null,
            }));
        }
    }
    serde_json::json!({ "commands": commands })
}

/// The mutable half of a session summary, as an AHP delta.
///
/// Only fields a store change can move are carried: the channel URI keys the
/// summary and the timestamps are the server's identity for it, so restating
/// them from a possibly-stale snapshot would be a regression rather than an
/// update.
pub(crate) fn summary_delta(
    summary: &SessionSummary,
) -> ahp_types::notifications::PartialSessionSummary {
    ahp_types::notifications::PartialSessionSummary {
        provider: Some(summary.provider.clone()),
        title: Some(summary.title.clone()),
        status: Some(summary.status),
        activity: summary.activity.clone(),
        origin: summary.origin.clone(),
        project: summary.project.clone(),
        working_directories: summary.working_directories.clone(),
        annotations: summary.annotations.clone(),
        ..Default::default()
    }
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
        created_at: unix_to_rfc3339(row.created_at),
        modified_at: unix_to_rfc3339(row.updated_at),
        changes: None,
        meta: None,
    }
}

impl Backend for RuntimeBackend {
    fn root_state(&self) -> RootState {
        root::with_agents(self.agents(), Some(self.server.terminal_infos()))
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
        use ahp_types::state as ahp;
        let snapshot = self.server.terminal_state(terminal_id)?;
        Some(ahp::TerminalState {
            title: snapshot.title,
            cwd: snapshot.cwd,
            cols: Some(snapshot.cols),
            rows: Some(snapshot.rows),
            // The visible grid is what a late subscriber must have. AHP allows a
            // command/output split (`TerminalContentPart::Command`); this runtime
            // does not track command boundaries, so the whole screen is one
            // unclassified part rather than a fabricated split.
            content: vec![ahp::TerminalContentPart::Unclassified(
                ahp::TerminalUnclassifiedPart {
                    value: snapshot.lines.join("\n"),
                },
            )],
            lifecycle: match snapshot.exit_code {
                Some(code) => {
                    ahp::TerminalLifecycleState::Exited(ahp::TerminalExitedLifecycleState {
                        exit_code: Some(code),
                    })
                }
                None => ahp::TerminalLifecycleState::Running(ahp::TerminalRunningLifecycleState {}),
            },
            // One process-local host serves every client, and the runtime does not
            // arbitrate input ownership, so the claim is the session's: announcing
            // a client claim we would not enforce invites two clients to type into
            // one PTY.
            claim: ahp::TerminalClaim::Session(ahp::TerminalSessionClaim {
                session: manox_ahp::channels::session::uri(&snapshot.session_id),
                chat: manox_ahp::channels::session::uri(&snapshot.session_id),
                turn_id: None,
                tool_call_id: None,
            }),
            supports_command_detection: Some(false),
            is_pty: Some(true),
        })
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
            .create_terminal(session_id, terminal_id, cols, rows)
            .map_err(|error| HostError::Backend(error.message))?;
        self.ensure_terminal_pump(terminal_id);
        self.publish_terminals_changed();
        Ok(())
    }

    #[cfg(feature = "terminal")]
    fn dispose_terminal(&self, terminal_id: &str) -> Result<(), HostError> {
        self.server
            .dispose_terminal(terminal_id)
            .map_err(|_| HostError::NotFound(manox_ahp::channels::terminal::uri(terminal_id)))?;
        self.publish_terminals_changed();
        Ok(())
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
        let intent = crate::runtime_trait::SessionIntent {
            session_id: Some(session_id.to_string()),
            cwd: working_directories.first().cloned(),
            project: None,
            initial_model: None,
            approval_mode: None,
            reasoning_effort: None,
            seed: None,
            working_directories,
        };
        self.server
            .create_session(&owner, intent)
            .map_err(|error| HostError::Backend(error.message))?;
        // Seed and bridge the new session now, so a client that subscribes after
        // creating it gets a snapshot and then live actions.
        let _ = block_on(self.seeded(session_id));
        Ok(())
    }

    fn dispose_session(&self, session_id: &str) -> Result<(), HostError> {
        let _ = self.server.dispose_session("ahp", session_id);
        // The engine caches this session's directories and full patch
        // bodies; a disposed session must not keep them.
        super::changeset::Engine::global().forget(session_id);
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
        let intent = crate::runtime_trait::ForkIntent {
            source_session_id: source_session_id.to_string(),
            through_entry_id: through_entry_id.to_string(),
            target_session_id: Some(chat_id.to_string()),
            cwd: None,
            project: None,
            initial_model: None,
            approval_mode: None,
            reasoning_effort: None,
        };
        self.server
            .fork_session(&owner, intent)
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
        let _ = self.server.dispose_session("ahp", chat_id);
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
                let target = session_id.to_string();
                let session_id = target.clone();
                match self.server.submit(&owner, &target, text) {
                    Ok(_) => {
                        // The host folds the client's turn-start so the turn id
                        // the client holds IS the host's active turn: the bridge
                        // then streams every part against it (`open_with_id`),
                        // and the dispatching client gets the echo its
                        // optimistic UI retires against. Without this the bridge
                        // had to mint its own synthetic turnStarted whenever the
                        // host had no active turn — a race against the engine's
                        // first content row, which the client's reducer silently
                        // dropped on every loss (a whole turn invisible).
                        if let Some(host) = self.host() {
                            host.publish(channel, action.clone(), None);
                        }
                        // A submit materializes the engine, so a session that was
                        // cold when the client subscribed has no bridge yet: the
                        // first `seeded` ran before the engine existed and its
                        // `ensure_bridge` bailed. Re-seed (cached) and start the
                        // bridge now that the engine is live.
                        let _ = block_on(self.seeded(&session_id));
                        self.ensure_bridge(&session_id);
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
                let target = session_id.to_string();
                let receipt = match self.server.steer(&target, &set.id, text) {
                    Ok(receipt) => receipt,
                    Err(error) => return DispatchOutcome::Rejected(error.message),
                };
                // An injected steer is consumed by the turn it interrupted, and
                // AHP clears `steeringMessage` only when a matching removal
                // arrives — nothing clears it on turn end. So the host owes that
                // removal the moment the injection is confirmed; without it the
                // client keeps rendering a steer that is already in the
                // transcript, and a later turn inherits the residue.
                if receipt.get("injected").and_then(Value::as_bool) == Some(true)
                    && let Some(host) = self.host()
                {
                    host.publish(
                        channel,
                        StateAction::ChatPendingMessageRemoved(ChatPendingMessageRemovedAction {
                            kind: ahp_types::state::PendingMessageKind::Steering,
                            id: set.id.clone(),
                        }),
                        None,
                    );
                }
                DispatchOutcome::Accepted
            }
            StateAction::ChatPendingMessageRemoved(removed) => {
                let Some(session_id) = chat::id(channel) else {
                    return DispatchOutcome::Rejected(format!("no runtime intent for {channel}"));
                };
                // Both kinds name a parked follow-up the runtime holds by id, so
                // withdrawing one is the same intent either way.
                self.server.drop_queued(session_id, &removed.id);
                DispatchOutcome::Accepted
            }
            StateAction::ChatTurnCancelled(_) => {
                let Some(session_id) = chat::id(channel) else {
                    return DispatchOutcome::Rejected(format!("no runtime intent for {channel}"));
                };
                match self.server.cancel_turn(session_id) {
                    Ok(()) => DispatchOutcome::Accepted,
                    Err(error) => DispatchOutcome::Rejected(error.message),
                }
            }
            StateAction::ChatToolCallConfirmed(confirmed) => self.confirm_tool_call(
                channel,
                &confirmed.tool_call_id,
                confirmed.meta.as_ref(),
                confirmed.approved,
            ),
            StateAction::ChatInputCompleted(completed) => {
                if completed.request_id.starts_with("plan-review:") {
                    self.resolve_plan_review(channel, completed)
                } else {
                    self.answer_question(
                        channel,
                        &completed.request_id,
                        map_answers(&completed.answers),
                    )
                }
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
                match self.server.set_cwd(session_id, &path) {
                    Ok(()) => DispatchOutcome::Accepted,
                    Err(error) => DispatchOutcome::Rejected(error.message),
                }
            }
            // `session/activeClientSet` is how an AHP client joins a session and
            // publishes the tools it contributes. AHP carries the tool set on
            // the client's own entry (`SessionState.activeClients[].tools`), so
            // registration needs no `x-manox` channel: the action *is* the
            // registration, and the host's reducer folds it into the very state
            // a subscriber reads. The runtime half is the same registration
            // store the v2 `RegisterSessionTools` call fills, so a tool a
            // client contributes here becomes callable by the model through the
            // one existing path (`embedder_tools`), not a second one.
            StateAction::SessionActiveClientSet(set) => {
                let Some(session_id) = session::id(channel) else {
                    return DispatchOutcome::Rejected(format!("no runtime intent for {channel}"));
                };
                match self.register_client_tools(session_id, &set.active_client) {
                    Ok(()) => DispatchOutcome::Accepted,
                    Err(reason) => DispatchOutcome::Rejected(reason),
                }
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
                use crate::runtime_trait::RenameOutcome;
                match self.server.rename_session(session_id, &changed.title) {
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
            // Review is client-dispatchable and the engine is the authority:
            // record the flags so a recompute carries (or resets) them
            // correctly. An unknown changeset or file set is a refusal, never
            // a folded phantom.
            StateAction::ChangesetFilesReviewChanged(changed) => {
                let Some((session_id, key)) = super::changeset::parse(channel) else {
                    return DispatchOutcome::Rejected(format!("no runtime intent for {channel}"));
                };
                match super::changeset::Engine::global().review(
                    &session_id,
                    key,
                    &changed.files,
                    changed.reviewed,
                ) {
                    Ok(()) => DispatchOutcome::Accepted,
                    Err(reason) => DispatchOutcome::Rejected(reason),
                }
            }
            // MCP start/stop: the acceptance is the *intent*, not the outcome.
            // The reducer has already folded the optimistic `starting`/`stopped`
            // edge, so the runtime owes the settled state through the MCP event
            // stream — a failure there publishes `session/mcpServerStateChanged`
            // (error/stopped) rather than retroactively rejecting what every
            // subscriber already folded. Only an unknown server id or a runtime
            // without the MCP plane is refused up front.
            StateAction::SessionMcpServerStartRequested(requested) => {
                let Some(session_id) = session::id(channel) else {
                    return DispatchOutcome::Rejected(format!("no runtime intent for {channel}"));
                };
                match self.server.mcp_start(session_id, &requested.id) {
                    Ok(()) => DispatchOutcome::Accepted,
                    Err(error) => DispatchOutcome::Rejected(error.message),
                }
            }
            StateAction::SessionMcpServerStopRequested(requested) => {
                let Some(session_id) = session::id(channel) else {
                    return DispatchOutcome::Rejected(format!("no runtime intent for {channel}"));
                };
                match self.server.mcp_stop(session_id, &requested.id) {
                    Ok(()) => DispatchOutcome::Accepted,
                    Err(error) => DispatchOutcome::Rejected(error.message),
                }
            }
            // The toggle's decisive decision is its first entry (senders sort
            // enablement by descending specificity, and the spec names
            // `enablement[0]` decisive). manox's registry is process-global,
            // so a session- or workspace-scoped decision degrades to the
            // same global toggle — recorded in the PR's Assumptions. An
            // absent or unrecognised decision is refused, not defaulted:
            // this is a write path, and guessing "enable" would fold a state
            // the client did not ask for.
            StateAction::SessionCustomizationToggled(toggled) => {
                let Some(session_id) = session::id(channel) else {
                    return DispatchOutcome::Rejected(format!("no runtime intent for {channel}"));
                };
                use ahp_types::state::CustomizationEnablement;
                let decision = match toggled.enablement.first() {
                    Some(CustomizationEnablement::Global { enabled })
                    | Some(CustomizationEnablement::Workspace { enabled, .. })
                    | Some(CustomizationEnablement::Session { enabled }) => Some(*enabled),
                    Some(CustomizationEnablement::Unknown(_)) | None => None,
                };
                let Some(enabled) = decision else {
                    return DispatchOutcome::Rejected(
                        "the toggle carries no recognisable enablement decision".to_string(),
                    );
                };
                match self
                    .server
                    .mcp_set_enabled(session_id, &toggled.id, enabled)
                {
                    Ok(()) => DispatchOutcome::Accepted,
                    Err(error) => DispatchOutcome::Rejected(error.message),
                }
            }
            // The config is file-owned (mcp.toml / plugin manifests); a
            // client-side mutation of anything beyond enablement would fold a
            // customization the next launch would not reproduce.
            StateAction::SessionCustomizationUpdated(_)
            | StateAction::SessionCustomizationRemoved(_) => DispatchOutcome::Rejected(
                "MCP customization entries are file-owned; only enablement is mutable".to_string(),
            ),
            // ── terminal actions ───────────────────────────────────────────
            //
            // A build without the terminal plane has no PTY to drive; it refuses
            // loudly rather than folding an action nothing acted on.
            #[cfg(feature = "terminal")]
            StateAction::TerminalInput(input) => {
                let Some(terminal_id) = terminal::id(channel) else {
                    return DispatchOutcome::Rejected(format!("no runtime intent for {channel}"));
                };
                match self.server.terminal_input(terminal_id, &input.data) {
                    Ok(()) => DispatchOutcome::Accepted,
                    Err(error) => DispatchOutcome::Rejected(error.message),
                }
            }
            #[cfg(feature = "terminal")]
            StateAction::TerminalResized(resized) => {
                let Some(terminal_id) = terminal::id(channel) else {
                    return DispatchOutcome::Rejected(format!("no runtime intent for {channel}"));
                };
                // AHP sizes are plain integers; the PTY's are `u16`. A value
                // outside that range is a client bug, and clamping silently
                // would resize to something the client did not ask for.
                let (Ok(cols), Ok(rows)) =
                    (u16::try_from(resized.cols), u16::try_from(resized.rows))
                else {
                    return DispatchOutcome::Rejected(format!(
                        "terminal size out of range: {}x{}",
                        resized.cols, resized.rows
                    ));
                };
                match self.server.terminal_resize(terminal_id, cols, rows) {
                    Ok(()) => DispatchOutcome::Accepted,
                    Err(error) => DispatchOutcome::Rejected(error.message),
                }
            }
            #[cfg(not(feature = "terminal"))]
            StateAction::TerminalInput(_) | StateAction::TerminalResized(_) => {
                DispatchOutcome::Rejected(
                    "terminal support is not built into this host".to_string(),
                )
            }
            // AHP types this as client-owned observation state with no runtime
            // effect: the claim is the protocol's, and this host does not
            // arbitrate input between clients (see `ahp_terminal_state`).
            StateAction::TerminalClaimed(_) => DispatchOutcome::Ignored,
            // ── x-manox: pin and manual ordering ──────────────────────────
            //
            // AHP has no pin bit and no ordering field (verified against
            // `SessionStatus`, `SessionMetadata` and `SessionSummary`), so both
            // ride the declared extension surface. Their durable authority is
            // the store row plus `sidebar_order`, the same layers v2's
            // `PinThread` / `InsertThreadBefore` use.
            StateAction::Unknown(tag)
                if tag.get("type").and_then(Value::as_str)
                    == Some(manox_ahp::ext::actions::PINNED_CHANGED) =>
            {
                let Some(session_id) = session::id(channel) else {
                    return DispatchOutcome::Rejected(format!("no runtime intent for {channel}"));
                };
                let Some(pinned) = tag.get("pinned").and_then(Value::as_bool) else {
                    return DispatchOutcome::Rejected(
                        "x-manox/pinnedChanged needs a boolean `pinned`".to_string(),
                    );
                };
                if self.server.pin_session(session_id, pinned) {
                    DispatchOutcome::Accepted
                } else {
                    DispatchOutcome::Rejected(format!("unknown session: {session_id}"))
                }
            }
            StateAction::Unknown(tag)
                if tag.get("type").and_then(Value::as_str)
                    == Some(manox_ahp::ext::actions::ORDER_CHANGED) =>
            {
                let Some(session_id) = session::id(channel) else {
                    return DispatchOutcome::Rejected(format!("no runtime intent for {channel}"));
                };
                // `before: null` means "to the head of its partition", which is
                // what the store's `None` anchor means.
                let before = match tag.get("before") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(anchor)) => Some(anchor.as_str()),
                    Some(_) => {
                        return DispatchOutcome::Rejected(
                            "x-manox/orderChanged `before` must be a session id or null"
                                .to_string(),
                        );
                    }
                };
                if self.server.order_session(session_id, before) {
                    DispatchOutcome::Accepted
                } else {
                    DispatchOutcome::Rejected(format!("unknown session: {session_id}"))
                }
            }
            StateAction::SessionIsArchivedChanged(changed) => {
                let Some(session_id) = session::id(channel) else {
                    return DispatchOutcome::Rejected(format!("no runtime intent for {channel}"));
                };
                self.server
                    .archive_session(&origin.client_id, session_id, changed.is_archived);
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

    fn changeset_state(&self, channel: &str) -> Option<ahp_types::state::ChangesetState> {
        let (session_id, key) = super::changeset::parse(channel)?;
        // First sight: run the scan (the catalogue call ensures the engine
        // entry), then answer from it.
        let dirs = block_on(async { seed_directories(&session_id).await });
        if let Some(dirs) = dirs {
            super::changeset::Engine::global().catalogue(&session_id, dirs);
        }
        super::changeset::Engine::global().state(&session_id, key)
    }

    fn invoke_changeset_operation(
        &self,
        channel: &str,
        operation_id: &str,
        target: Option<&ahp_types::commands::ChangesetOperationTarget>,
    ) -> Result<Value, HostError> {
        use ahp_types::commands::InvokeChangesetOperationResult;
        let (session_id, key) = super::changeset::parse(channel)
            .ok_or_else(|| HostError::NotFound(channel.to_string()))?;
        if operation_id != "revert" {
            return Err(HostError::InvalidParams(format!(
                "unknown changeset operation: {operation_id}"
            )));
        }
        let Some(host) = self.host() else {
            return Err(HostError::Unimplemented(
                "changeset without a host".to_string(),
            ));
        };
        // Running → work → settled, every edge on the changeset channel so
        // all subscribers see the same spinner and the same outcome.
        host.publish(
            channel,
            StateAction::ChangesetOperationStatusChanged(
                ahp_types::actions::ChangesetOperationStatusChangedAction {
                    operation_id: operation_id.to_string(),
                    status: ahp_types::state::ChangesetOperationStatus::Running,
                    error: None,
                },
            ),
            None,
        );
        let outcome = super::changeset::Engine::global().invoke_revert(&session_id, key, target);
        match outcome {
            Ok(message) => {
                for (channel, action) in super::changeset::Engine::global().recompute(&session_id) {
                    host.publish(&channel, action, None);
                }
                host.publish(
                    channel,
                    StateAction::ChangesetOperationStatusChanged(
                        ahp_types::actions::ChangesetOperationStatusChangedAction {
                            operation_id: operation_id.to_string(),
                            status: ahp_types::state::ChangesetOperationStatus::Idle,
                            error: None,
                        },
                    ),
                    None,
                );
                serde_json::to_value(InvokeChangesetOperationResult {
                    message: Some(message.into()),
                    follow_up: None,
                })
                .map_err(|e| HostError::Backend(e.to_string()))
            }
            Err(reason) => {
                host.publish(
                    channel,
                    StateAction::ChangesetOperationStatusChanged(
                        ahp_types::actions::ChangesetOperationStatusChangedAction {
                            operation_id: operation_id.to_string(),
                            status: ahp_types::state::ChangesetOperationStatus::Error,
                            error: Some(ahp_types::state::ErrorInfo {
                                error_type: "changeset".to_string(),
                                message: reason.clone(),
                                stack: None,
                                meta: None,
                            }),
                        },
                    ),
                    None,
                );
                Err(HostError::Backend(reason))
            }
        }
    }

    fn mcp_channel_request(
        &self,
        channel: &str,
        method: &str,
        params: &Value,
    ) -> Result<Value, HostError> {
        #[cfg(feature = "mcp")]
        {
            let Some(key) = manox_ahp::channels::mcp::server(channel) else {
                return Err(HostError::InvalidParams(format!(
                    "not an mcp:// channel: {channel}"
                )));
            };
            let Some(connected) = manox_agent::mcp::try_global().and_then(|registry| {
                registry
                    .servers()
                    .into_iter()
                    .find(|server| server.name == key)
            }) else {
                // A stopped or never-started server has no channel — the
                // customization's cleared `channel` is the honest signal.
                return Err(HostError::NotFound(channel.to_string()));
            };
            block_on(async { proxy_mcp_method(connected.client.peer(), method, params).await })
        }
        #[cfg(not(feature = "mcp"))]
        {
            let _ = (channel, method, params);
            Err(HostError::Unimplemented(format!("mcp channel: {method}")))
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
    fn extension_baseline(&self, channel: &str) -> Option<Value> {
        if !manox_ahp::ext::is_extension_channel(channel) {
            return None;
        }
        // The per-session channels fold one session's journal; the catalogue
        // channels (`x-manox-workspaces://`, `x-manox-commands://`) describe the
        // host and carry no session id.
        let state =
            if let Some(session_id) = channel.strip_prefix(manox_ahp::ext::channels::METRICS) {
                // The metrics channel's baseline is the Q-face aggregate, not the
                // per-row ext fold: a subscriber must see the same read model the
                // bridge pushes, from the first frame on. A cold session (no live
                // engine) has no fold yet — an empty aggregate, not a refusal:
                // the channel is served, the first bridge frame fills it.
                self.server
                    .conversation_metrics(session_id)
                    .unwrap_or_else(|| serde_json::json!({}))
            } else if manox_ahp::ext::is_session_scoped_channel(channel) {
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
                        .map(|state| serde_json::to_value(state).unwrap_or_else(|_| Value::Null))
                        // A declared channel with no rows yet answers its (empty)
                        // state, not `null`: the client replaces what it holds,
                        // and `null` would claim the channel says nothing at all.
                        .unwrap_or_else(|| serde_json::json!({})),
                    // An unknown session is not a served channel: answering an
                    // empty baseline would claim state for a session that has none.
                    None => return None,
                }
            } else {
                self.catalogue_baseline(channel)
            };
        Some(state)
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
                if !self.server.has_session(&session_id) {
                    return Err(HostError::SessionNotFound(session_id));
                }
                self.server
                    .compact(&session_id, instructions)
                    .map_err(|error| HostError::Backend(error.message))?;
                Ok(Value::Null)
            }
            manox_ahp::ext::commands::PLAN_EXECUTE => {
                let session_id = extension_session(params)?;
                let Some(plan_file) = params.get("planFile").and_then(Value::as_str) else {
                    return Err(HostError::InvalidParams(
                        "x-manox/planExecute needs planFile".to_string(),
                    ));
                };
                if !self.server.has_session(&session_id) {
                    return Err(HostError::SessionNotFound(session_id));
                }
                self.server
                    .plan_seed(&session_id, plan_file)
                    .map_err(|error| HostError::Backend(error.message))?;
                Ok(Value::Null)
            }
            manox_ahp::ext::commands::GOAL => {
                let session_id = extension_session(params)?;
                // `action` is required: a goal command with no verb has no
                // meaning, and defaulting it would make a malformed request
                // look like a lifecycle step that did nothing.
                let Some(action) = params.get("action").and_then(Value::as_str) else {
                    return Err(HostError::InvalidParams(
                        "x-manox/goal needs action".to_string(),
                    ));
                };
                if !self.server.has_session(&session_id) {
                    return Err(HostError::SessionNotFound(session_id));
                }
                let objective = params
                    .get("objective")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let budget = params.get("budget").and_then(Value::as_u64);
                let max_rounds = params.get("maxRounds").and_then(Value::as_u64);
                self.server
                    .goal(&session_id, action, objective, budget, max_rounds)
                    .map_err(|error| HostError::Backend(error.message))?;
                Ok(Value::Null)
            }
            // A client asked for a surface this build declares but does not
            // perform. `-32080` tells it apart from "you sent a bad request".
            other => Err(HostError::Unimplemented(other.to_string())),
        }
    }
}

/// One client-contributed tool as the runtime's registration store holds it.
///
/// AHP's `ToolDefinition` carries the schema as optional (a client tool need
/// not declare one) but the engine's `ClientToolSpec` requires it, so an absent
/// schema becomes the permissive empty object — the model then calls the tool
/// with no constraints, which is what "no schema declared" means. Everything
/// else maps straight across; the `readOnlyHint` annotation is the registrant's
/// advisory side-effect hint, and the approval gate stays the authority (the
/// same rule MCP tools follow).
fn client_tool_spec(
    tool: &ahp_types::state::ToolDefinition,
) -> Result<crate::runtime_trait::ClientToolSpec, String> {
    if tool.name.trim().is_empty() {
        return Err("a contributed tool needs a name".to_string());
    }
    Ok(crate::runtime_trait::ClientToolSpec {
        name: tool.name.clone(),
        description: tool.description.clone().unwrap_or_default(),
        input_schema: tool
            .input_schema
            .clone()
            .unwrap_or_else(|| serde_json::json!({ "type": "object" })),
        read_only: tool
            .annotations
            .as_ref()
            .and_then(|annotations| annotations.read_only_hint)
            .unwrap_or(false),
    })
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
pub(crate) fn file_uri_to_path(uri: &str) -> Option<String> {
    uri.strip_prefix("file://").map(str::to_string)
}

/// The session's working directories as paths — the changeset engine's scan
/// roots, resolved the same way the seed's `workingDirectories` are.
async fn seed_directories(session_id: &str) -> Option<Vec<std::path::PathBuf>> {
    let dirs = super::seed_working_directories(session_id).await?;
    Some(
        dirs.iter()
            .filter_map(|directory| file_uri_to_path(directory))
            .map(std::path::PathBuf::from)
            .collect(),
    )
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

/// Map AHP's completed-card answers onto the kernel's `AskAnswer` list.
///
/// A select answer carries its label; a selected-many answer its labels; a
/// text/number/boolean answer becomes free-form `custom`. A skipped question
/// contributes an explicit skip — the kernel's empty-selection-no-custom shape
/// — because a skip is a decision the model should see, not an absence.
fn map_answers(
    answers: &Option<std::collections::HashMap<String, ahp_types::state::ChatInputAnswer>>,
) -> Vec<manox_agent::permission::AskAnswer> {
    let Some(answers) = answers else {
        return Vec::new();
    };
    answers
        .iter()
        .filter_map(|(id, answer)| {
            let value = match answer {
                ahp_types::state::ChatInputAnswer::Draft(a)
                | ahp_types::state::ChatInputAnswer::Submitted(a) => &a.value,
                // A skip is a decision the user made, not an absent answer, and
                // the kernel encodes it as exactly this: an empty selection with
                // no `custom`. Returning `None` here dropped the row entirely,
                // so the model saw a question that was neither answered nor
                // skipped — it could not tell "the user skipped this" from "the
                // client never sent it".
                ahp_types::state::ChatInputAnswer::Skipped(skipped) => {
                    // A skip may carry a reason, and the kernel's `custom` is
                    // where free text belongs — so a "skip, because …" reaches
                    // the model as a skip *with* that reason rather than as a
                    // bare skip.
                    return Some(manox_agent::permission::AskAnswer {
                        id: id.clone(),
                        selected: Vec::new(),
                        custom: skipped
                            .freeform_values
                            .as_ref()
                            .and_then(|values| values.first().cloned()),
                    });
                }
            };
            match value {
                ahp_types::state::ChatInputAnswerValue::Selected(v) => {
                    Some(manox_agent::permission::AskAnswer {
                        id: id.clone(),
                        selected: vec![v.value.clone()],
                        custom: v.freeform_values.as_ref().and_then(|f| f.first().cloned()),
                    })
                }
                ahp_types::state::ChatInputAnswerValue::SelectedMany(v) => {
                    Some(manox_agent::permission::AskAnswer {
                        id: id.clone(),
                        selected: v.value.clone(),
                        custom: v.freeform_values.as_ref().and_then(|f| f.first().cloned()),
                    })
                }
                ahp_types::state::ChatInputAnswerValue::Text(v) => {
                    Some(manox_agent::permission::AskAnswer {
                        id: id.clone(),
                        selected: Vec::new(),
                        custom: Some(v.value.clone()),
                    })
                }
                ahp_types::state::ChatInputAnswerValue::Number(v) => {
                    Some(manox_agent::permission::AskAnswer {
                        id: id.clone(),
                        selected: Vec::new(),
                        custom: Some(v.value.to_string()),
                    })
                }
                ahp_types::state::ChatInputAnswerValue::Boolean(v) => {
                    Some(manox_agent::permission::AskAnswer {
                        id: id.clone(),
                        selected: Vec::new(),
                        custom: Some(v.value.to_string()),
                    })
                }
                _ => None,
            }
        })
        .collect()
}

/// A store timestamp (Unix seconds) as RFC 3339, or empty when out of range.
fn unix_to_rfc3339(secs: i64) -> String {
    chrono::TimeZone::timestamp_opt(&chrono::Utc, secs, 0)
        .single()
        .map(|t| t.to_rfc3339())
        .unwrap_or_default()
}

#[cfg(test)]
mod answer_mapping_tests {
    use super::map_answers;
    use ahp_types::state::{
        ChatInputAnswer, ChatInputAnswerValue, ChatInputAnswered, ChatInputSelectedAnswerValue,
        ChatInputSkipped, ChatInputTextAnswerValue,
    };

    fn answers(
        rows: Vec<(&str, ChatInputAnswer)>,
    ) -> Option<std::collections::HashMap<String, ChatInputAnswer>> {
        Some(
            rows.into_iter()
                .map(|(id, answer)| (id.to_string(), answer))
                .collect(),
        )
    }

    fn submitted(value: ChatInputAnswerValue) -> ChatInputAnswer {
        ChatInputAnswer::Submitted(ChatInputAnswered { value })
    }

    #[test]
    fn a_skipped_question_is_an_explicit_skip_not_a_dropped_row() {
        let mapped = map_answers(&answers(vec![(
            "q1",
            ChatInputAnswer::Skipped(ChatInputSkipped {
                freeform_values: None,
            }),
        )]));
        assert_eq!(
            mapped.len(),
            1,
            "the skip must reach the kernel: {mapped:?}"
        );
        assert_eq!(mapped[0].id, "q1");
        assert!(
            mapped[0].selected.is_empty() && mapped[0].custom.is_none(),
            "the kernel's skip shape is an empty selection with no custom: {:?}",
            mapped[0]
        );
    }

    #[test]
    fn a_selected_label_rides_through_verbatim() {
        let mapped = map_answers(&answers(vec![(
            "q1",
            submitted(ChatInputAnswerValue::Selected(
                ChatInputSelectedAnswerValue {
                    value: "Approve".to_string(),
                    freeform_values: None,
                },
            )),
        )]));
        assert_eq!(mapped.len(), 1);
        assert_eq!(mapped[0].selected, vec!["Approve".to_string()]);
        assert_eq!(mapped[0].custom, None);
    }

    #[test]
    fn free_text_lands_as_custom_with_no_selection() {
        let mapped = map_answers(&answers(vec![(
            "q1",
            submitted(ChatInputAnswerValue::Text(ChatInputTextAnswerValue {
                value: "use the fast suite".to_string(),
            })),
        )]));
        assert_eq!(mapped.len(), 1);
        assert!(mapped[0].selected.is_empty());
        assert_eq!(mapped[0].custom.as_deref(), Some("use the fast suite"));
    }

    #[test]
    fn no_answers_at_all_is_an_empty_list() {
        assert!(map_answers(&None).is_empty());
    }

    /// The approve path keys on the option's **identifier**, which is what a
    /// client echoes back in `selected` (`ChatInputOption::id` is documented as
    /// the stable option identifier). The plan-review card's option id and the
    /// runtime's comparison have to stay the same string or an approved plan
    /// silently fails to seed — a mismatch here is invisible, because the
    /// dispatch still answers `Accepted`.
    #[test]
    fn an_approved_plan_selection_is_recognised_by_option_id() {
        let mapped = map_answers(&answers(vec![(
            "plan-review:e-1:q",
            submitted(ChatInputAnswerValue::Selected(
                ChatInputSelectedAnswerValue {
                    value: "approve".to_string(),
                    freeform_values: None,
                },
            )),
        )]));
        assert!(
            mapped
                .iter()
                .any(|a| a.selected.iter().any(|s| s == "approve")),
            "the runtime's approve test must match what the card offers: {mapped:?}"
        );
    }
}

/// The dispatch arms for MCP start/stop answer from the runtime seam, so the
/// contract lives at the seam: an unknown id (or a runtime without the MCP
/// plane, whose trait defaults refuse) is a refusal, a served intent is
/// accepted and forwarded verbatim.
#[cfg(test)]
mod mcp_dispatch_tests {
    use super::*;
    use crate::error::RuntimeError;
    use crate::runtime_trait::{
        ClientToolSpec, ForkIntent, RenameOutcome, SessionIntent, TerminalSnapshot,
    };

    /// A runtime that answers only the MCP seam and records what it was asked;
    /// every other method is unreachable from these tests.
    struct McpOnlyRuntime {
        served: bool,
        started: Mutex<Vec<String>>,
        stopped: Mutex<Vec<String>>,
        enabled: Mutex<Vec<(String, bool)>>,
    }

    impl McpOnlyRuntime {
        fn refusing() -> Arc<Self> {
            Arc::new(Self {
                served: false,
                started: Mutex::new(Vec::new()),
                stopped: Mutex::new(Vec::new()),
                enabled: Mutex::new(Vec::new()),
            })
        }

        fn serving() -> Arc<Self> {
            Arc::new(Self {
                served: true,
                started: Mutex::new(Vec::new()),
                stopped: Mutex::new(Vec::new()),
                enabled: Mutex::new(Vec::new()),
            })
        }
    }

    impl SessionRuntime for McpOnlyRuntime {
        fn terminal_state(&self, _terminal_id: &str) -> Option<TerminalSnapshot> {
            None
        }
        fn terminal_raw_tap(
            &self,
            _terminal_id: &str,
        ) -> Option<tokio::sync::broadcast::Receiver<std::sync::Arc<Vec<u8>>>> {
            None
        }
        fn create_terminal(
            &self,
            _session_id: &str,
            _terminal_id: &str,
            _cols: u16,
            _rows: u16,
        ) -> Result<(), RuntimeError> {
            Err(RuntimeError::new("unused"))
        }
        fn dispose_terminal(&self, _terminal_id: &str) -> Result<(), RuntimeError> {
            Err(RuntimeError::new("unused"))
        }
        fn terminal_input(&self, _terminal_id: &str, _data: &str) -> Result<(), RuntimeError> {
            Err(RuntimeError::new("unused"))
        }
        fn terminal_resize(
            &self,
            _terminal_id: &str,
            _cols: u16,
            _rows: u16,
        ) -> Result<(), RuntimeError> {
            Err(RuntimeError::new("unused"))
        }
        fn create_session(&self, _owner: &str, _intent: SessionIntent) -> Result<(), RuntimeError> {
            Err(RuntimeError::new("unused"))
        }
        fn fork_session(&self, _owner: &str, _intent: ForkIntent) -> Result<(), RuntimeError> {
            Err(RuntimeError::new("unused"))
        }
        fn dispose_session(&self, _owner: &str, _session_id: &str) -> Result<(), RuntimeError> {
            Err(RuntimeError::new("unused"))
        }
        fn has_session(&self, _session_id: &str) -> bool {
            false
        }
        fn submit(
            &self,
            _owner: &str,
            _session_id: &str,
            _text: String,
        ) -> Result<Value, RuntimeError> {
            Err(RuntimeError::new("unused"))
        }
        fn steer(
            &self,
            _session_id: &str,
            _message_id: &str,
            _text: String,
        ) -> Result<Value, RuntimeError> {
            Err(RuntimeError::new("unused"))
        }
        fn drop_queued(&self, _session_id: &str, _message_id: &str) {}
        fn cancel_turn(&self, _session_id: &str) -> Result<(), RuntimeError> {
            Err(RuntimeError::new("unused"))
        }
        fn set_model(&self, _session_id: &str, _model: &str) -> Result<(), RuntimeError> {
            Err(RuntimeError::new("unused"))
        }
        fn set_reasoning_effort(
            &self,
            _session_id: &str,
            _effort: &str,
        ) -> Result<(), RuntimeError> {
            Err(RuntimeError::new("unused"))
        }
        fn set_approval_mode(&self, _session_id: &str, _mode: &str) -> Result<(), RuntimeError> {
            Err(RuntimeError::new("unused"))
        }
        fn set_cwd(&self, _session_id: &str, _cwd: &str) -> Result<(), RuntimeError> {
            Err(RuntimeError::new("unused"))
        }
        fn archive_session(&self, _owner: &str, _session_id: &str, _archived: bool) {}
        fn rename_session(&self, _session_id: &str, _title: &str) -> RenameOutcome {
            RenameOutcome::UnknownSession
        }
        fn pin_session(&self, _session_id: &str, _pinned: bool) -> bool {
            false
        }
        fn order_session(&self, _session_id: &str, _before: Option<&str>) -> bool {
            false
        }
        fn compact(
            &self,
            _session_id: &str,
            _instructions: Option<String>,
        ) -> Result<(), RuntimeError> {
            Err(RuntimeError::new("unused"))
        }
        fn plan_seed(&self, _session_id: &str, _plan_file: &str) -> Result<(), RuntimeError> {
            Err(RuntimeError::new("unused"))
        }
        fn goal(
            &self,
            _session_id: &str,
            _action: &str,
            _objective: Option<String>,
            _budget: Option<u64>,
            _max_rounds: Option<u64>,
        ) -> Result<(), RuntimeError> {
            Err(RuntimeError::new("unused"))
        }
        fn confirm_tool_call(
            &self,
            _session_id: &str,
            _auth_id: &str,
            _approved: bool,
        ) -> Result<(), RuntimeError> {
            Err(RuntimeError::new("unused"))
        }
        fn answer_question(
            &self,
            _session_id: &str,
            _request_id: &str,
            _answers: Vec<manox_agent::permission::AskAnswer>,
        ) -> Result<(), RuntimeError> {
            Err(RuntimeError::new("unused"))
        }
        fn journal_feed(&self, _session_id: &str) -> Option<manox_agent::thread::ThreadHandle> {
            None
        }
        fn set_embedder_tools(
            &self,
            _session_id: &str,
            _client_id: &str,
            _tools: Vec<ClientToolSpec>,
        ) {
        }

        fn mcp_start(&self, _session_id: &str, id: &str) -> Result<(), RuntimeError> {
            if !self.served {
                return Err(RuntimeError::new(
                    "MCP start is not supported by this runtime",
                ));
            }
            self.started.lock().push(id.to_string());
            Ok(())
        }

        fn mcp_stop(&self, _session_id: &str, id: &str) -> Result<(), RuntimeError> {
            if !self.served {
                return Err(RuntimeError::new(
                    "MCP stop is not supported by this runtime",
                ));
            }
            self.stopped.lock().push(id.to_string());
            Ok(())
        }

        fn mcp_set_enabled(
            &self,
            _session_id: &str,
            id: &str,
            enabled: bool,
        ) -> Result<(), RuntimeError> {
            if !self.served {
                return Err(RuntimeError::new(
                    "MCP enablement is not supported by this runtime",
                ));
            }
            self.enabled.lock().push((id.to_string(), enabled));
            Ok(())
        }
    }

    fn dispatch_start(backend: &RuntimeBackend, channel: &str, id: &str) -> DispatchOutcome {
        backend.dispatch(
            channel,
            &StateAction::SessionMcpServerStartRequested(
                ahp_types::actions::SessionMcpServerStartRequestedAction { id: id.to_string() },
            ),
            &ActionOrigin {
                client_id: "client-1".to_string(),
                client_seq: 1,
            },
        )
    }

    fn dispatch_stop(backend: &RuntimeBackend, channel: &str, id: &str) -> DispatchOutcome {
        backend.dispatch(
            channel,
            &StateAction::SessionMcpServerStopRequested(
                ahp_types::actions::SessionMcpServerStopRequestedAction { id: id.to_string() },
            ),
            &ActionOrigin {
                client_id: "client-1".to_string(),
                client_seq: 2,
            },
        )
    }

    fn backend(runtime: Arc<McpOnlyRuntime>) -> Arc<RuntimeBackend> {
        RuntimeBackend::new(runtime as Arc<dyn SessionRuntime>, std::env::temp_dir())
    }

    #[test]
    fn a_start_on_a_runtime_without_the_mcp_plane_is_a_refusal() {
        let backend = backend(McpOnlyRuntime::refusing());
        let outcome = dispatch_start(&backend, "ahp-session:/s-1", "github");
        assert!(
            matches!(&outcome, DispatchOutcome::Rejected(reason) if reason.contains("not supported")),
            "the refusal must reach the client: {outcome:?}"
        );
    }

    #[test]
    fn a_stop_on_a_runtime_without_the_mcp_plane_is_a_refusal() {
        let backend = backend(McpOnlyRuntime::refusing());
        let outcome = dispatch_stop(&backend, "ahp-session:/s-1", "github");
        assert!(matches!(outcome, DispatchOutcome::Rejected(_)));
    }

    #[test]
    fn a_start_intent_is_accepted_and_forwarded_with_its_id() {
        let runtime = McpOnlyRuntime::serving();
        let backend = backend(Arc::clone(&runtime));
        assert_eq!(
            dispatch_start(&backend, "ahp-session:/s-1", "github"),
            DispatchOutcome::Accepted
        );
        assert_eq!(*runtime.started.lock(), vec!["github".to_string()]);
    }

    #[test]
    fn a_stop_intent_is_accepted_and_forwarded_with_its_id() {
        let runtime = McpOnlyRuntime::serving();
        let backend = backend(Arc::clone(&runtime));
        assert_eq!(
            dispatch_stop(&backend, "ahp-session:/s-1", "github"),
            DispatchOutcome::Accepted
        );
        assert_eq!(*runtime.stopped.lock(), vec!["github".to_string()]);
    }

    #[test]
    fn an_mcp_intent_off_the_session_channel_is_rejected() {
        let backend = backend(McpOnlyRuntime::serving());
        assert!(matches!(
            dispatch_start(&backend, "ahp-root://", "github"),
            DispatchOutcome::Rejected(_)
        ));
    }

    fn dispatch_toggle(backend: &RuntimeBackend, id: &str, enabled: bool) -> DispatchOutcome {
        use ahp_types::state::CustomizationEnablement;
        backend.dispatch(
            "ahp-session:/s-1",
            &StateAction::SessionCustomizationToggled(
                ahp_types::actions::SessionCustomizationToggledAction {
                    id: id.to_string(),
                    enablement: vec![CustomizationEnablement::Global { enabled }],
                },
            ),
            &ActionOrigin {
                client_id: "client-1".to_string(),
                client_seq: 3,
            },
        )
    }

    #[test]
    fn a_toggle_reaches_the_runtime_with_its_decisive_value() {
        let runtime = McpOnlyRuntime::serving();
        let backend = backend(Arc::clone(&runtime));
        assert_eq!(
            dispatch_toggle(&backend, "github", false),
            DispatchOutcome::Accepted
        );
        assert_eq!(*runtime.enabled.lock(), vec![("github".to_string(), false)]);
    }

    #[test]
    fn a_toggle_on_a_runtime_without_the_mcp_plane_is_a_refusal() {
        let backend = backend(McpOnlyRuntime::refusing());
        assert!(matches!(
            dispatch_toggle(&backend, "github", true),
            DispatchOutcome::Rejected(_)
        ));
    }

    #[test]
    fn a_session_scoped_decision_still_carries_its_enabled_value() {
        use ahp_types::state::CustomizationEnablement;
        let runtime = McpOnlyRuntime::serving();
        let backend = backend(Arc::clone(&runtime));
        backend.dispatch(
            "ahp-session:/s-1",
            &StateAction::SessionCustomizationToggled(
                ahp_types::actions::SessionCustomizationToggledAction {
                    id: "fs".to_string(),
                    enablement: vec![
                        CustomizationEnablement::Session { enabled: false },
                        CustomizationEnablement::Global { enabled: true },
                    ],
                },
            ),
            &ActionOrigin {
                client_id: "client-1".to_string(),
                client_seq: 4,
            },
        );
        // The first entry is decisive per the wire contract (descending
        // specificity); manox degrades the scope to global but honors the
        // value.
        assert_eq!(*runtime.enabled.lock(), vec![("fs".to_string(), false)]);
    }

    #[test]
    fn a_toggle_without_a_recognisable_decision_is_refused_not_defaulted() {
        let backend = backend(McpOnlyRuntime::serving());
        let outcome = backend.dispatch(
            "ahp-session:/s-1",
            &StateAction::SessionCustomizationToggled(
                ahp_types::actions::SessionCustomizationToggledAction {
                    id: "github".to_string(),
                    enablement: Vec::new(),
                },
            ),
            &ActionOrigin {
                client_id: "client-1".to_string(),
                client_seq: 6,
            },
        );
        assert!(
            matches!(&outcome, DispatchOutcome::Rejected(reason) if reason.contains("no recognisable")),
            "an empty decision is a refusal, never a defaulted enable: {outcome:?}"
        );
    }

    #[test]
    fn customization_mutations_beyond_enablement_are_refused() {
        use ahp_types::state::{
            Customization, McpServerCustomization, McpServerReadyState, McpServerState,
        };
        let backend = backend(McpOnlyRuntime::serving());
        let customization = Customization::McpServer(Box::new(McpServerCustomization {
            id: "fs".to_string(),
            uri: "file:///mcp.toml".to_string(),
            name: "fs".to_string(),
            icons: None,
            range: None,
            meta: None,
            enablement: None,
            state: McpServerState::Ready(McpServerReadyState {}),
            channel: None,
            mcp_app: None,
        }));
        let outcome = backend.dispatch(
            "ahp-session:/s-1",
            &StateAction::SessionCustomizationUpdated(Box::new(
                ahp_types::actions::SessionCustomizationUpdatedAction { customization },
            )),
            &ActionOrigin {
                client_id: "client-1".to_string(),
                client_seq: 5,
            },
        );
        assert!(
            matches!(&outcome, DispatchOutcome::Rejected(reason) if reason.contains("file-owned")),
            "the refusal must name the ownership rule: {outcome:?}"
        );
    }
}
