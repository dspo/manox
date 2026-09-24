//! AgentServer — the single protocol gateway.
//!
//! The only public surface between frontends and the gpui-free kernel: every
//! client (the gpui desktop in-process, and any WS-gateway or napi host)
//! speaks [`manox_protocol`] over
//! an [`RpcConnection`], and the server drives kernel [`ThreadHandle`]s from
//! those messages. Kernel [`ThreadEvent`]s are projected through
//! [`manox_ahp_runtime::translate`] into [`ServerNote`] (streamed to the owning client) or
//! [`ServerCall`] (a round-trip the owning ∩ capable client must answer), so
//! the kernel stays free of transport and frontend concerns.
//!
//! Scope: connection/handshake, session ownership, the full
//! `ClientCall`/`ClientNote` dispatch, the `Note` event pump, and the
//! event-driven `ServerCall` round-trips — `Approve` (β-3a) plus
//! `AskUserQuestion` (β-3b-i; the plan review rides the same channel,
//! pump-initiated on PlanReady). `CapabilityClient` rewiring
//! (BrowserOp/ClipboardRead/OpenExternal), terminal, and model_chat are
//! β-3b-ii.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use manox_ahp_runtime::runtime_trait::{ClientToolSpec, ImageAttachment};
use manox_journal::base64_bytes;
use parking_lot::Mutex;
use serde_json::{Value, json};

use manox_agent::language_model::{MessageContent, ReasoningEffort};
use manox_agent::thread::{PermissionMode, ThreadHandle};
use manox_agent::thread_engine::BackendNotice;
use manox_agent::{MessageUiMetadata, Thread, ThreadEvent, ThreadId};
use manox_harness::session::jsonl::{JsonlSessionMetadata, JsonlSessionStorage};

use manox_ahp_runtime::journal_query;

/// How long the server waits for a client to answer a `ServerCall` before
/// treating it as fail-closed. Generous: a human reviewing a plan or an
/// approval may take minutes. The kernel never sets its own timeout — that
/// would duplicate the peer's correlation/timeout machinery.
const CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// One live session: the strong `ThreadHandle` (the retention owner) and the
/// turn bookkeeping the runtime intents read.
struct ServerSession {
    thread: ThreadHandle,
    turn_active: Arc<AtomicBool>,
    // parking_lot (round 4 §3.2): a panic inside a lock holder must not
    // poison the queue into a permanent cascade — the std .unwrap() locks
    // turned one panic into every subsequent submit/steer/drain panicking.
    pending_submits: Arc<Mutex<Vec<QueuedSubmit>>>,
}

/// One live wire terminal (#13): the gpui-free [`TerminalHandle`] owns the
/// PTY and the grid.
#[cfg(feature = "terminal")]
struct TerminalEntry {
    handle: manox_terminal::TerminalHandle,
    /// The session this terminal was attached for: the cwd source and the db
    /// row's owner.
    session_id: String,
    cwd: String,
    exited: StdMutex<Option<i32>>,
}

/// A submission parked while a turn runs; drained into one follow-up turn when
/// the turn settles, mirroring the legacy host's queued-follow-up behavior.
struct QueuedSubmit {
    client_id: String,
    text: String,
    images: Vec<(String, String)>,
    ui: MessageUiMetadata,
    /// The Submit's origin RPC id (echo retirement, §F.2). A drained batch
    /// merges into one turn, so the last non-None origin wins.
    origin: Option<String>,
}

/// The single gateway. Cloning shares the inner state.
pub struct AgentServer(Arc<AgentServerInner>);

/// Releases one transient session owner when its scope ends — success,
/// failure or panic — so an internal open can never leave a ghost owner
/// that defeats the orphaned-session reap (review r3 [sugg] 3).
struct OwnerLease {
    inner: Arc<AgentServerInner>,
    owner: &'static str,
    id: String,
}

impl Drop for OwnerLease {
    fn drop(&mut self) {
        self.inner.remove_owner(self.owner, &self.id);
    }
}

pub(crate) struct AgentServerInner {
    cwd: PathBuf,
    /// Bind redirect map (predecessor → successor session id): the live
    /// half of the supersede contract; the sidecar marker is the
    /// restart-surviving half (`ThreadStore::superseded_by`).
    superseded: Mutex<HashMap<String, String>>,
    /// Bind hand-offs in flight per predecessor: two quick `SetCwd` notes
    /// must not mint two successors (review #805 [sugg] 6).
    binding: Mutex<HashSet<String>>,
    sessions: Mutex<HashMap<String, ServerSession>>,
    /// session_id → client_ids that own (view) it. A session may have several
    /// owners; each receives its streamed notes.
    session_owners: Mutex<HashMap<String, Vec<String>>>,
    /// Live wire terminals (#13): id → entry. Spawned by `TerminalAttach`,
    /// fed by per-stream forwarder tasks, mirrored into the threads db and
    /// the `TerminalsUpdated` host snapshot.
    #[cfg(feature = "terminal")]
    terminals: Mutex<HashMap<String, Arc<TerminalEntry>>>,
    /// In-flight bare-model completions by request id (the LanguageModelChat
    /// provider path); cancellation tokens shared with the spawned streams.
    model_chats: Arc<StdMutex<HashMap<String, tokio_util::sync::CancellationToken>>>,
    /// Monotonically increasing counter for client entry generations, used to
    /// detect stale entries during same-client-id reconnection.
    next_generation: AtomicU64,
    /// §E.3 Q-face cache: `(thread_id, cursor)` → the folded conversation
    /// info payload (recomputed only when the cursor advances).
    conversation_info_cache: Arc<StdMutex<journal_query::ConversationInfoCache>>,
    /// Embedder tool registrations (RegisterSessionTools): session →
    /// client → full-replacement tool set. Consulted by the
    /// EmbedderToolProvider below each time the engine assembles a
    /// session's tools.
    embedder_tools: Mutex<HashMap<String, HashMap<String, Vec<ClientToolSpec>>>>,
}

impl AgentServerInner {
    /// Insert only when the key is absent, under ONE lock hold.
    ///
    /// The create path's live-session check and its insert were two lock
    /// acquisitions, so a racing same-id create/open could pass both checks
    /// and build two entries. This recheck adopts the entry that won the race
    /// and hands the loser back to its caller, so the loser's intent seeds can
    /// never overwrite the winner's model / approval / effort.
    fn insert_session_if_absent(
        &self,
        session_id: String,
        session: ServerSession,
    ) -> Option<ServerSession> {
        let mut sessions = self.sessions.lock();
        if sessions.contains_key(&session_id) {
            return Some(session);
        }
        sessions.insert(session_id, session);
        None
    }

    // ── #13 wire terminals. ─────────────────────────────────────────────────

    /// `TerminalAttach`: re-attach by id or spawn a fresh shell terminal
    /// bound to the session's cwd. Response carries the id + a text
    /// snapshot of the visible grid.
    #[cfg(feature = "terminal")]
    pub(crate) fn attach_terminal(
        self: &Arc<Self>,
        session_id: &str,
        cols: u16,
        rows: u16,
        terminal_id: Option<String>,
    ) -> Result<Value, manox_ahp_runtime::error::RuntimeError> {
        if let Some(id) = terminal_id.clone() {
            let existing = self.terminals.lock().get(&id).cloned();
            if let Some(entry) = existing {
                return Ok(self.terminal_attach_response(&entry));
            }
        }
        let id = terminal_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let cwd = self
            .session_thread(session_id)
            .map(|t| t.read(|t| t.cwd().to_path_buf()))
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let pty = manox_terminal::pty::open(&cwd, cols, rows, None, &[]).map_err(|e| {
            manox_ahp_runtime::error::RuntimeError::new(format!("terminal spawn failed: {e}"))
                .with_code(manox_ahp_runtime::error::codes::GATEWAY_INTERNAL)
        })?;
        let handle = manox_terminal::Terminal::spawn(
            id.clone(),
            cwd.clone(),
            cols as usize,
            rows as usize,
            Box::new(pty),
        )
        .map_err(|e| {
            manox_ahp_runtime::error::RuntimeError::new(format!("terminal spawn failed: {e}"))
                .with_code(manox_ahp_runtime::error::codes::GATEWAY_INTERNAL)
        })?;
        let entry = Arc::new(TerminalEntry {
            handle,
            session_id: session_id.to_string(),
            cwd: cwd.to_string_lossy().into_owned(),
            exited: StdMutex::new(None),
        });
        self.terminals.lock().insert(id.clone(), entry.clone());
        self.upsert_terminal_db(&entry, None);
        Ok(self.terminal_attach_response(&entry))
    }

    /// The raw PTY byte tap of a terminal, for the AHP output pump.
    ///
    /// Raw rather than the decoded event stream: `terminal/data` carries the
    /// process's own bytes, and the pump is where a byte-fragment stream is
    /// reassembled into text at character boundaries.
    #[cfg(feature = "terminal")]
    pub(crate) fn terminal_raw_tap(
        &self,
        terminal_id: &str,
    ) -> Option<tokio::sync::broadcast::Receiver<std::sync::Arc<Vec<u8>>>> {
        self.terminals
            .lock()
            .get(terminal_id)
            .map(|entry| entry.handle.subscribe_raw())
    }

    /// `terminal/input`: forward keystrokes to a terminal's PTY.
    ///
    /// AHP types this as a side-effect-only action (the reducer no-ops it), so
    /// the only observable effect is what the PTY does with the bytes — which
    /// comes back on the wire as `terminal/data`.
    #[cfg(feature = "terminal")]
    pub(crate) fn terminal_input(&self, terminal_id: &str, data: &str) -> Result<(), String> {
        let Some(entry) = self.terminals.lock().get(terminal_id).cloned() else {
            return Err(format!("unknown terminal {terminal_id}"));
        };
        entry
            .handle
            .read(|t| t.input(data.as_bytes()))
            .map_err(|error| format!("terminal input failed: {error}"))
    }

    /// `terminal/resized`: resize a terminal's PTY grid.
    #[cfg(feature = "terminal")]
    pub(crate) fn terminal_resize(
        &self,
        terminal_id: &str,
        cols: u16,
        rows: u16,
    ) -> Result<(), String> {
        let Some(entry) = self.terminals.lock().get(terminal_id).cloned() else {
            return Err(format!("unknown terminal {terminal_id}"));
        };
        entry
            .handle
            .with_mut(|t| t.resize(cols as usize, rows as usize));
        Ok(())
    }

    /// `DisposeTerminal`: release a terminal.
    ///
    /// Dropping the entry is the release — the PTY handle reaps its child on
    /// `Drop` — so the removed entry must actually be dropped here rather than
    /// left to a lingering watcher clone. That is why the map removal is the
    /// last statement and the value is not retained.
    #[cfg(feature = "terminal")]
    pub(crate) fn dispose_terminal(&self, terminal_id: &str) -> bool {
        let Some(entry) = self.terminals.lock().remove(terminal_id) else {
            return false;
        };
        // The drop is the release: the PTY handle reaps its child on `Drop`.
        drop(entry);
        true
    }

    /// `TerminalSnapshot`: the visible grid as text lines + cursor.
    #[cfg(feature = "terminal")]
    fn terminal_snapshot(&self, terminal_id: &str) -> Option<Value> {
        let entry = self.terminals.lock().get(terminal_id).cloned()?;
        Some(self.terminal_snapshot_value(&entry))
    }

    #[cfg(feature = "terminal")]
    fn terminal_snapshot_value(&self, entry: &TerminalEntry) -> Value {
        let (lines, cursor_col, cursor_row) = entry.handle.read(|t| t.text_snapshot());
        let (cols, rows) = entry.handle.read(|t| (t.cols, t.rows));
        serde_json::json!({
            "cols": cols,
            "rows": rows,
            "cursor": { "x": cursor_col, "y": cursor_row },
            "lines": lines,
        })
    }

    #[cfg(feature = "terminal")]
    fn terminal_attach_response(&self, entry: &TerminalEntry) -> Value {
        let id = entry.handle.read(|t| t.id.clone());
        serde_json::json!({
            "terminal_id": id,
            "snapshot": self.terminal_snapshot_value(entry),
        })
    }

    /// The AHP view of one live terminal, for the `ahp-terminal:/<id>` channel.
    ///
    /// Unlike the v2 summary this is the channel's *state*, so it carries the
    /// visible grid: a subscriber that arrives late needs the screen, not just
    /// the fact that a terminal exists.
    #[cfg(feature = "terminal")]
    pub(crate) fn ahp_terminal_state(
        &self,
        terminal_id: &str,
    ) -> Option<ahp_types::state::TerminalState> {
        use ahp_types::state as ahp;
        let entry = self.terminals.lock().get(terminal_id).cloned()?;
        let (lines, cols, rows, title, cwd) = entry.handle.read(|t| {
            let (lines, _, _) = t.text_snapshot();
            (lines, t.cols, t.rows, t.title.clone(), t.cwd.clone())
        });
        let exited = *entry.exited.lock().unwrap();
        Some(ahp::TerminalState {
            title: title.unwrap_or_default(),
            cwd: cwd.to_str().map(str::to_string),
            cols: Some(cols as i64),
            rows: Some(rows as i64),
            // The visible grid is what a late subscriber must have. AHP allows a
            // command/output split (`TerminalContentPart::Command`); this slice
            // does not track command boundaries, so the whole screen is one
            // unclassified part rather than a fabricated split.
            content: vec![ahp::TerminalContentPart::Unclassified(
                ahp::TerminalUnclassifiedPart {
                    value: lines.join("\n"),
                },
            )],
            lifecycle: match exited {
                Some(code) => {
                    ahp::TerminalLifecycleState::Exited(ahp::TerminalExitedLifecycleState {
                        exit_code: Some(code as i64),
                    })
                }
                None => ahp::TerminalLifecycleState::Running(ahp::TerminalRunningLifecycleState {}),
            },
            // One process-local host serves every client, and the runtime does
            // not arbitrate input ownership, so `Watched` is the honest claim:
            // announcing a client claim we do not enforce would invite two
            // clients to type into one PTY.
            claim: ahp::TerminalClaim::Session(ahp::TerminalSessionClaim {
                session: manox_ahp_runtime::ahp::session_uri_of_terminal(&entry.session_id),
                chat: manox_ahp_runtime::ahp::session_uri_of_terminal(&entry.session_id),
                turn_id: None,
                tool_call_id: None,
            }),
            supports_command_detection: Some(false),
            is_pty: Some(true),
        })
    }


    #[cfg(feature = "terminal")]
    fn upsert_terminal_db(&self, entry: &TerminalEntry, exit: Option<i32>) {
        let Ok(path) = manox_agent::db::default_db_path() else {
            return;
        };
        let Ok(db) = manox_agent::db::ThreadsDatabase::open(&path) else {
            return;
        };
        let id = entry.handle.read(|t| t.id.clone());
        let title = entry.handle.read(|t| t.title.clone());
        let now = chrono::Utc::now().timestamp_millis();
        let _ = exit; // exit code rides the summary, not the row
        let _ = db.upsert_terminal_session(&manox_agent::db::TerminalSession {
            id,
            cwd: entry.cwd.clone(),
            env: Vec::new(),
            title,
            created_at: now,
            updated_at: now,
        });
    }

}

/// Install the process-wide embedder-tool provider backed by this server.
///
/// This is the host wiring the engine's tool assembly consults: without it
/// `engine`'s `embedder_tools::provider()` is `None` in a production build,
/// so tools a client registers via `RegisterSessionTools` are stored but
/// never reach the model's tool set and `invokeClientTool` never fires.
///
/// #803 first installed this on the `global()` singleton alone, and the
/// coverage was incomplete: the napi edge (`crates/manox-napi/src/lib.rs`,
/// `start`) builds its server through `AgentServer::new` and never routes
/// through `global` — so the VS Code host kept `provider() == None` and
/// client-registered tools stayed invisible to the model (reproduced with a
/// headless addon probe: `{registered: 1}` yet no `client_*` tool in the
/// model's tool enumeration). The original carve-out — a private-server
/// embedder "keeping ownership of the provider slot" — never described a
/// shipping host: every in-repo embedder needs the provider, and one now
/// gets it by constructing a server.
///
/// The install therefore lives in the shared constructor (`new_inner`):
/// EVERY build path — `global()` (the desktop and the `ws` gateway), the
/// napi binding, and the test-only `new_without_store_watcher` — installs
/// its own provider. `set_provider` stays last-wins by design (see
/// `manox_agent::embedder_tools::set_provider`): each production host runs
/// exactly one server per process (`global`'s OnceLock; napi's `start`
/// rejects a second construction through its connection slot), so the slot
/// always resolves to the live server, and an embedder that genuinely wants
/// to own the slot can still re-set it after construction — last-wins is
/// the escape hatch. Tests mutate the slot under the `lock_globals` suite
/// mutex, so no concurrent construction can clobber a registration
/// mid-assertion.
fn install_embedder_provider(server: &AgentServer) {
    manox_agent::embedder_tools::set_provider(std::sync::Arc::new(AgentServerEmbedderTools::new(
        server,
    )));
}

/// The process-global server (L11: one `AgentServer` per process — the
/// desktop, the embedded web UI and every future frontend route through it,
/// so ownership/routing tables are shared). First caller wins; later cwd
/// arguments are ignored (a second window shares the first window's cwd).
pub fn global(cwd: std::path::PathBuf) -> std::sync::Arc<AgentServer> {
    static GLOBAL: std::sync::OnceLock<std::sync::Arc<AgentServer>> = std::sync::OnceLock::new();
    // No provider install here (#803 follow-up): it moved into `new_inner`,
    // where it covers this singleton AND the direct `AgentServer::new`
    // paths (napi included) — see [`install_embedder_provider`].
    let server = GLOBAL
        .get_or_init(|| std::sync::Arc::new(AgentServer::new(cwd)))
        .clone();
    // The AHP runtime half cannot construct a session runtime itself — that is
    // this gateway's business, and naming one there would recreate the
    // dependency the split exists to remove. So the owner installs the builder.
    // Last-wins is refused: a process has one session store, and letting two
    // embedders race would give the AHP face a runtime other than the one its
    // clients are talking to.
    if !AHP_BUILDER_INSTALLED.swap(true, Ordering::SeqCst) {
        let _ = manox_ahp_runtime::ahp::runtime::install_builder(|cwd| {
            Arc::new(crate::ahp_gateway::GatewayRuntime::new(global(cwd)))
                as Arc<dyn manox_ahp_runtime::runtime_trait::SessionRuntime>
        });
    }
    server
}

/// Whether this process already installed the AHP runtime builder.
static AHP_BUILDER_INSTALLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

impl AgentServer {
    pub fn new(cwd: PathBuf) -> Self {
        Self::new_inner(cwd, true)
    }

    /// Test-only constructor WITHOUT the U6a store watcher: the
    /// strict-frame-sequence tests keep deterministic streams (the
    /// watcher's list-refresh broadcasts are pinned by their dedicated
    /// regression, `store_change_broadcasts_the_list_refresh`, through
    /// `harness_with_store_watcher`).
    #[cfg(test)]
    pub fn new_without_store_watcher(cwd: PathBuf) -> Self {
        Self::new_inner(cwd, false)
    }

    fn new_inner(cwd: PathBuf, store_watcher: bool) -> Self {
        // #13: the terminal pumps need a runtime; first registration wins
        // and the agent runtime is already live here.
        #[cfg(feature = "terminal")]
        manox_terminal::runtime::set_runtime(manox_agent::runtime::handle().clone());
        let inner = Arc::new(AgentServerInner {
            cwd,
            superseded: Mutex::new(HashMap::new()),
            binding: Mutex::new(HashSet::new()),
            sessions: Mutex::new(HashMap::new()),
            session_owners: Mutex::new(HashMap::new()),
            #[cfg(feature = "terminal")]
            terminals: Mutex::new(HashMap::new()),
            model_chats: Arc::new(StdMutex::new(HashMap::new())),
            next_generation: AtomicU64::new(1),
            conversation_info_cache: Arc::new(StdMutex::new(
                journal_query::ConversationInfoCache::default(),
            )),
            embedder_tools: Mutex::new(HashMap::new()),
        });
        // U2 cross-domain #2 (§D.5 Models: pushed immediately on provider reload): a
        // provider reload broadcasts the fresh snapshot to every
        // connection. Weak, so a dropped server leaves an inert listener;
        // a newer server re-registers (last one wins).
        manox_agent::provider_glue::set_reload_listener(Some(Box::new({
            let weak = Arc::downgrade(&inner);
            move || {
                if let Some(inner) = weak.upgrade() {
                    inner.broadcast_models_after_reload();
                }
            }
        })));
        // U6a (§D.5 ThreadsUpdated — full snapshot on metadata change — made real): the
        // store-event watcher owns the list-refresh broadcast — any store
        // summary write (title auto-stamps, interacted_at bumps, pin/
        // archive/tag, rescans) reaches EVERY connection as the
        // ThreadsUpdated push, so no client needs an in-process store
        // subscription to keep its list fresh (the desktop's store-event
        // bridge retired against this). Weak like the reload listener: a
        // dropped server leaves the watcher inert, and the store's channel
        // close (teardown) retires the task. A store-less construction
        // (foreign fixtures) skips the watcher — there is nothing to watch.
        if store_watcher && let Some(store) = manox_agent::thread_store::try_global() {
            let rx = store.subscribe();
            let weak = Arc::downgrade(&inner);
            manox_agent::runtime::handle().spawn(async move {
                use manox_agent::thread_store::ThreadStoreEvent;
                while let Ok(ev) = rx.recv().await {
                    // Coalesce bursts (a bulk rescan or the startup refresh
                    // pushes one event per write): one broadcast covers the
                    // drained batch. RunningChanged is skipped — the running
                    // column rides the §D.5 SessionStatus deltas.
                    let mut summaries = matches!(*ev, ThreadStoreEvent::SummariesUpdated);
                    while let Ok(more) = rx.try_recv() {
                        summaries |= matches!(*more, ThreadStoreEvent::SummariesUpdated);
                    }
                    if !summaries {
                        continue;
                    }
                    let Some(inner) = weak.upgrade() else {
                        return;
                    };
                    inner.broadcast_threads_after_store_change();
                }
            });
        }
        let server = Self(inner);
        // The host wiring the engine's tool assembly consults: install
        // THIS server's embedder-tool provider. Lives in the shared
        // constructor — not in `global` — so every build path (desktop/ws
        // through `global`, the napi binding through `AgentServer::new`,
        // test fixtures) wires the provider; last-wins makes the newest
        // construction authoritative. See [`install_embedder_provider`].
        install_embedder_provider(&server);
        // The engine's window into the frontend (browser, clipboard, opener).
        // Also last-wins, for the same one-server-per-process reason.
        manox_agent::capability::set_provider(std::sync::Arc::new(
            AgentServerCapabilityClient::new(&server),
        ));
        server
    }

    /// The AHP host dispatches into the same inner intents the socket surfaces
    /// use: one write path, one implementation, no second copy to drift.
    /// one write path, two protocols, no second implementation to drift.
    pub(crate) fn ahp_inner(&self) -> &Arc<AgentServerInner> {
        &self.0
    }

    /// Test-only: the live session ids this server holds (the reap
    /// regression needs to observe an entry disappearing).
    #[cfg(test)]
    pub fn live_session_ids(&self) -> Vec<String> {
        self.0.sessions.lock().keys().cloned().collect()
    }

    /// Test-only: one session's owner ids (the hand-off must move the
    /// audience to the successor, not the reverse).
    #[cfg(test)]
    pub fn session_owners_for_test(&self, session_id: &str) -> Vec<String> {
        self.0.owners(session_id)
    }

    /// Test-only: set a scripted engine on a session before any turn runs, so
    /// the event pump can be exercised without a live provider.
    #[cfg(test)]
    pub fn set_session_engine_for_test(
        &self,
        session_id: &str,
        engine: Arc<dyn manox_agent::thread_engine::ThreadEngine>,
        events: tokio::sync::mpsc::UnboundedReceiver<BackendNotice>,
    ) {
        if let Some(thread) = self.0.session_thread(session_id) {
            thread.with_mut(|t| t.set_engine_for_test(engine, events));
        }
    }
}

impl AgentServerInner {
    // ── Pure state accessors (no spawning). ─────────────────────────────────
    /// Resolve the bind redirect chain: the live map first, the sidecar
    /// `superseded_by` marker as the restart-surviving half (cached into
    /// the live map on first hit). The superseded guard drops before the
    /// store read (lock order), and the bounded walk caps cycles.
    fn resolve_redirect(&self, session_id: &str) -> String {
        let mut id = session_id.to_string();
        for _ in 0..8 {
            let next = { self.superseded.lock().get(&id).cloned() };
            let next = next.or_else(|| {
                let from_store = manox_agent::thread_store::try_global()
                    .and_then(|store| store.read(|s| s.superseded_by(&id)));
                if let Some(succ) = &from_store {
                    self.superseded.lock().insert(id.clone(), succ.clone());
                }
                from_store
            });
            match next {
                Some(next) if next != id => id = next,
                _ => break,
            }
        }
        id
    }

    pub(crate) fn session_thread(&self, session_id: &str) -> Option<ThreadHandle> {
        let id = self.resolve_redirect(session_id);
        self.sessions.lock().get(&id).map(|s| s.thread.clone())
    }

    fn owners(&self, session_id: &str) -> Vec<String> {
        self.session_owners
            .lock()
            .get(session_id)
            .cloned()
            .unwrap_or_default()
    }

    fn add_owner(&self, session_id: &str, client_id: &str) {
        let mut owners = self.session_owners.lock();
        let list = owners.entry(session_id.to_string()).or_default();
        if !list.contains(&client_id.to_string()) {
            list.push(client_id.to_string());
        }
    }

    fn remove_owner(&self, client_id: &str, session_id: &str) {
        let mut owners = self.session_owners.lock();
        if let Some(list) = owners.get_mut(session_id) {
            list.retain(|c| c != client_id);
            if list.is_empty() {
                owners.remove(session_id);
            }
        }
    }

    // ── Catalogue push. ────────────────────────────────────────────────────
    //
    // The AHP host owns the root channel's catalogue, so a change to it is
    // published through the host rather than pushed down a socket. These two
    // hooks are the only reactions left to a store or provider change: the
    // gateway keeps no per-connection fan-out, and a client that misses a
    // notification re-lists.
    //
    // Both are no-ops when no AHP host is up (a headless embedder, or a test
    // that drives only the runtime intents), so the intents stay usable
    // without a protocol face.

    /// Republish the agent catalogue after a provider reload.
    fn broadcast_models_after_reload(&self) {
        let Some(runtime) = manox_ahp_runtime::ahp::runtime::try_runtime() else {
            return;
        };
        let agents = runtime.host().backend().root_state().agents;
        if agents.is_empty() {
            return;
        }
        publish_root_agents(&runtime.host(), agents);
    }

    /// Announce that the session catalogue moved (a session appeared, was
    /// renamed, pinned, reordered or archived).
    ///
    /// The adapter owns the delta vocabulary, so the gateway only raises the
    /// event; see `SessionRuntime::catalogue_changed`.
    fn broadcast_threads_after_store_change(&self) {
        let Some(runtime) = manox_ahp_runtime::ahp::runtime::try_runtime() else {
            return;
        };
        runtime.server().catalogue_changed();
    }

    /// Drop one session's embedder tool registrations. Called wherever the
    /// live session entry is removed — the registration is session-scoped
    /// and must not outlive the session.
    fn clear_embedder_tools(&self, session_id: &str) {
        self.embedder_tools.lock().remove(session_id);
    }

    /// Full-replacement write of one client's embedder tool set for a session.
    ///
    /// The AHP face (`session/activeClientSet`) lands here, exactly like the v2
    /// `RegisterSessionTools` call: both are the same registration fact, and
    /// the engine consults this one store when it assembles a session's tools.
    pub(crate) fn set_embedder_tools(
        &self,
        session_id: &str,
        client_id: &str,
        tools: Vec<ClientToolSpec>,
    ) {
        self.embedder_tools
            .lock()
            .entry(session_id.to_string())
            .or_default()
            .insert(client_id.to_string(), tools);
    }

}

/// Open (or re-own) a live session: load its journal, insert the entry, and
/// make `owner` an owner of it.
///
/// Idempotent in both directions — an already-live session is re-owned without
/// any IO, and two racing opens of a cold id converge on one entry (the loser
/// adopts the winner's `ThreadHandle` through the store's weak upgrade, so
/// nothing is loaded twice).
async fn open_session(
    inner: &Arc<AgentServerInner>,
    owner: &str,
    session_id: &str,
) -> Result<(), manox_ahp_runtime::error::RuntimeError> {
    // Phase 1 (fast path): a live session is re-owned without any IO.
    if inner.sessions.lock().contains_key(session_id) {
        inner.add_owner(session_id, owner);
        return Ok(());
    }
    // Phase 2 (U8): the journal-file IO runs OUTSIDE the `sessions` lock —
    // a slow disk must not stall the whole gateway table. Concurrent racers
    // load the SAME `ThreadHandle` (the store's weak upgrade), and only phase
    // 3 decides who inserts.
    let thread = manox_agent::thread_store::global()
        .with_mut(|s| s.load_thread(session_id))
        .map_err(|error| {
            // Another process drives this session: its engine actor holds
            // the per-session write lease. Fail fast — the client surfaces
            // the stable code and can retry after the holder exits.
            tracing::warn!(session_id, %error, "session open blocked by a foreign write lease");
            manox_ahp_runtime::error::RuntimeError::new(error.to_string())
                .with_code(manox_ahp_runtime::error::codes::SESSION_ALREADY_OWNED)
        })?
        .ok_or_else(|| unknown_session(session_id))?;
    // Phase 3: recheck–insert under ONE lock hold. The loser finds the
    // winner's entry and re-owns it, discarding its own load (the same handle
    // via the weak upgrade — nothing leaks).
    {
        let mut sessions = inner.sessions.lock();
        if !sessions.contains_key(session_id) {
            sessions.insert(
                session_id.to_string(),
                ServerSession {
                    thread: thread.clone(),
                    turn_active: Arc::new(AtomicBool::new(false)),
                    pending_submits: Arc::new(Mutex::new(Vec::new())),
                },
            );
        }
    }
    inner.add_owner(session_id, owner);
    Ok(())
}

/// No such live session.
///
/// Shared by the runtime intents that refuse with `session/notFound`, so the
/// code a client matches on does not depend on which intent refused.
fn unknown_session(session_id: &str) -> manox_ahp_runtime::error::RuntimeError {
    manox_ahp_runtime::error::RuntimeError::new(format!("unknown session: {session_id}"))
        .with_code(manox_ahp_runtime::error::codes::SESSION_NOT_FOUND)
}

/// No registered model answers to this id.
fn unresolvable_model(id: &str) -> manox_ahp_runtime::error::RuntimeError {
    manox_ahp_runtime::error::RuntimeError::new(format!("unknown model: {id}"))
        .with_code(manox_ahp_runtime::error::codes::MODEL_UNRESOLVABLE)
}

/// Carry a runtime failure across the async seam without losing its code.
///
/// The intents already classify their refusals; re-wrapping them with a generic
/// code would erase exactly the distinction a client matches on, so the only
/// thing this adds is the `Send`-safe ownership the spawned future needs.
pub(crate) fn preserve_code(
    error: manox_ahp_runtime::error::RuntimeError,
) -> manox_ahp_runtime::error::RuntimeError {
    error
}

/// Publish a fresh agent catalogue to the root channel's subscribers.
fn publish_root_agents(host: &Arc<manox_ahp::Host>, agents: Vec<ahp_types::state::AgentInfo>) {
    host.publish(
        ahp_types::common::ROOT_RESOURCE_URI,
        ahp_types::actions::StateAction::RootAgentsChanged(
            ahp_types::actions::RootAgentsChangedAction { agents },
        ),
        None,
    );
}

/// The mutable half of a session summary, as an AHP delta.
///
/// Only the fields a store change can move are carried: identity (channel,
/// createdAt, isRead) is not a "change", and leaving it absent lets the
/// client's merge keep the values it already holds rather than restating them
/// from a snapshot that may be a beat stale.
fn summary_delta(
    summary: &ahp_types::state::SessionSummary,
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

/// `ClientCall::ForkSession` (dspo/manox-app#9): create a new session whose
/// journal is the source's active chain up to (and including)
/// `through_entry_id`.
///
/// The fork is a prefix copy, not a cross-file redirect — `Leaf` targets are
/// file-local and the load-time chain validator rejects foreign parents — so
/// a fork materializes a fresh journal file: new id, new `thread` header
/// stamp (a copied stamp would collapse the fork into the source's sidebar
/// row), `parentSession` pointing back at the source file. The source's
/// active-chain rows are re-appended through the storage's own append path
/// (ids and parent links preserved; seq re-derived along the chain, which
/// for a dense prefix reproduces the source seqs exactly). Engine state
/// carried by the journal — model, cwd, goal, plan review, the subagent
/// rail — restores from the copied rows on load; nothing else is rewritten
/// (threads.db is not on the session-creation path). The source is read
/// straight off its persisted file (the cold path): appends are durable at
/// write time, so this is correct for live and cold sources alike, and a
/// deferred (never-materialized) source has no file and answers
/// `session/not-found`.
///
/// The intent fields override the inherited state ONLY when explicitly
/// given — absent fields inherit from the copied journal (unlike
/// `CreateSession`, no global-default model is applied). Overrides land on
/// the live thread after the open as durable change rows, exactly like the
/// `CreateSession` path's seeds. The prefix copies through the storage's
/// batched append (one lock hold, one validation pass, ONE file write):
/// O(chain) total — a whole-prefix validation failure rejects the fork
/// before any row touches disk.
pub(crate) use manox_ahp_runtime::runtime_trait::ForkIntent;

pub(crate) async fn fork_session(
    inner: &Arc<AgentServerInner>,
    owner: &str,
    intent: ForkIntent,
) -> Result<Value, manox_ahp_runtime::error::RuntimeError> {
    let ForkIntent {
        source_session_id,
        through_entry_id,
        target_session_id,
        cwd,
        project,
        initial_model,
        approval_mode,
        reasoning_effort,
    } = intent;
    // Resolve every intent field that can fail before touching the
    // filesystem (the same wire vocabularies CreateSession validates).
    let model = match initial_model.as_ref() {
        None => None,
        Some(m) => {
            let registry = manox_agent::provider_glue::global();
            match manox_harness::model_ref::resolve_model_ref(&registry, &m.0) {
                Some(model) => Some(model),
                None => {
                    return Err(manox_ahp_runtime::error::RuntimeError::new(format!("unknown model: {}", m.0))
                        .with_code(manox_ahp_runtime::error::codes::MODEL_UNRESOLVABLE));
                }
            }
        }
    };
    let approval = match approval_mode.as_deref() {
        None => None,
        Some(s) => match serde_json::from_value::<PermissionMode>(Value::String(s.to_string())) {
            Ok(mode) => Some(mode),
            Err(_) => {
                return Err(manox_ahp_runtime::error::RuntimeError::new(format!("unknown approval mode: {s}"))
                    .with_code(manox_ahp_runtime::error::codes::GATEWAY_BAD_REQUEST));
            }
        },
    };
    let effort = match reasoning_effort.as_deref() {
        None => None,
        Some("high") => Some(ReasoningEffort::High),
        Some("max") => Some(ReasoningEffort::Max),
        Some(other) => {
            return Err(
                manox_ahp_runtime::error::RuntimeError::new(format!("unknown reasoning effort: {other}"))
                    .with_code(manox_ahp_runtime::error::codes::GATEWAY_BAD_REQUEST),
            );
        }
    };

    // Source: the persisted file (cold path — valid for live and cold
    // sources; a deferred source has no file).
    let Some(source_path) = manox_ahp_runtime::paths::persisted_session_file(&source_session_id)
    else {
        return Err(manox_ahp_runtime::error::RuntimeError::new("thread not found")
            .with_code(manox_ahp_runtime::error::codes::SESSION_NOT_FOUND));
    };
    if !source_path.exists() {
        return Err(manox_ahp_runtime::error::RuntimeError::new("thread not found")
            .with_code(manox_ahp_runtime::error::codes::SESSION_NOT_FOUND));
    }
    let source = JsonlSessionStorage::open(&source_path)
        .await
        .map_err(|err| {
            manox_ahp_runtime::error::RuntimeError::new(format!("journal corrupt: {err}"))
                .with_code(manox_ahp_runtime::error::codes::GATEWAY_INTERNAL)
        })?;
    let records = source.journal_range(0, u64::MAX).await.map_err(|err| {
        manox_ahp_runtime::error::RuntimeError::new(format!("journal corrupt: {err}"))
            .with_code(manox_ahp_runtime::error::codes::GATEWAY_INTERNAL)
    })?;
    let through = records
        .iter()
        .position(|r| r.entry.id() == through_entry_id)
        .ok_or_else(|| {
            manox_ahp_runtime::error::RuntimeError::new(
                format!("entry {through_entry_id} is not on the source's active chain"),
            )
            .with_code(manox_ahp_runtime::error::codes::GATEWAY_BAD_REQUEST)
        })?;

    // Materialize the fork file immediately (never deferred — a non-empty
    // prefix must be visible to `list` and loadable cold).
    let session_id = target_session_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let Some(target_path) = manox_ahp_runtime::paths::persisted_session_file(&session_id) else {
        return Err(manox_ahp_runtime::error::RuntimeError::new("minted fork id failed the path gate")
            .with_code(manox_ahp_runtime::error::codes::GATEWAY_INTERNAL));
    };
    // A caller-chosen id must not silently overwrite an existing journal: the
    // fork file is created below, and `create` truncates. Refuse instead, so a
    // colliding request is a loud error rather than a destroyed session.
    if target_path.exists() {
        return Err(
            manox_ahp_runtime::error::RuntimeError::new(format!("session {session_id} already exists"))
                .with_code(manox_ahp_runtime::error::codes::GATEWAY_BAD_REQUEST),
        );
    }
    let fork_cwd = cwd.clone().unwrap_or_else(|| source.metadata.cwd.clone());
    let target = JsonlSessionStorage::create(
        &target_path,
        JsonlSessionMetadata {
            id: session_id.clone(),
            cwd: fork_cwd,
            created_at: chrono::Utc::now(),
            // `parentSession` stays a PathBuf to the JSON boundary
            // (review #775): a non-UTF8 source path now fails the fork
            // LOUDLY at header serialization instead of silently writing a
            // lossy-mangled link.
            parent_session_path: Some(source_path.clone()),
            metadata: Some(serde_json::json!({
                "host": manox_agent::host::current().slug(),
                "thread": session_id,
            })),
        },
    )
    .await
    .map_err(|err| {
        manox_ahp_runtime::error::RuntimeError::new(format!("fork file creation failed: {err}"))
            .with_code(manox_ahp_runtime::error::codes::GATEWAY_INTERNAL)
    })?;
    let prefix: Vec<manox_harness::session::SessionTreeEntry> = records[..=through]
        .iter()
        .map(|r| r.entry.clone())
        .collect();
    target.append_entries(&prefix).await.map_err(|err| {
        manox_ahp_runtime::error::RuntimeError::new(format!("fork row copy failed: {err}"))
            .with_code(manox_ahp_runtime::error::codes::GATEWAY_INTERNAL)
    })?;
    drop(target);

    // Register + open through the same path OpenSession takes (load, pump,
    // owner registration, SessionCreated note + Host mirror). The seed
    // makes the unscanned id loadable before any list refresh runs.
    manox_agent::thread_store::global()
        .with_mut(|s| s.note_session_path(&session_id, &target_path));
    open_session(inner, owner, &session_id).await?;

    // Explicit intent overrides only — absent fields keep the state the
    // copied journal restored (no global-default model here).
    let thread = inner
        .sessions
        .lock()
        .get(&session_id)
        .map(|entry| entry.thread.clone());
    if let Some(thread) = thread {
        thread.with_mut(|t| {
            if let Some(model) = model {
                t.set_model(model);
            }
            if let Some(mode) = approval {
                t.set_permission_mode(mode);
            }
            if let Some(effort) = effort {
                t.set_reasoning_effort(effort);
            }
            if let Some(project) = project {
                t.bind_at_creation(PathBuf::from(project));
            }
        });
    }
    Ok(json!({ "session_id": session_id }))
}

// ── Per-command handlers (&self methods, no spawning). ────────────────────────

/// The §D.2 `CreateSession` intent: optional explicit id (the compat
/// `ClientNote::CreateSession` always supplies one; the v2 request mints
/// server-side), working directory, project binding, and the initial
/// model / approval mode / reasoning effort the session opens with (the
/// "project/model inheritance" defect regression, §J.7).
pub(crate) use manox_ahp_runtime::runtime_trait::SessionIntent;

impl AgentServerInner {
    /// §D.2 `CreateSession`: build a live session from the intent and answer
    /// `{session_id}`. The thread opens on the `new_in_project` path when a
    /// project is given (fresh session bound to the project in one step,
    /// no orphaned pre-project file), else `new_fresh`; `initial_model`
    /// resolves through the single convergence point
    /// `resolve_model_ref` (L8) *before* anything is created — an
    /// unresolvable canonical ref answers `model/unresolvable` without a
    /// side effect. Re-opening a live session id is idempotent: the
    /// existing id answers and the live session is left untouched. An id
    /// whose journal file already exists on disk is an existing COLD
    /// session: it restores through the `OpenSession` path (§D.2
    /// idempotency on disk, GW11) and is never re-minted over its file.
    pub(crate) async fn create_session_request(
        inner: &Arc<AgentServerInner>,
        owner: &str,
        intent: SessionIntent,
    ) -> Result<Value, manox_ahp_runtime::error::RuntimeError> {
        // Resolve every intent field that can fail before touching state.
        let model = match intent.initial_model.as_ref() {
            None => None,
            Some(m) => {
                let registry = manox_agent::provider_glue::global();
                match manox_harness::model_ref::resolve_model_ref(&registry, &m.0) {
                    Some(model) => Some(model),
                    None => {
                        return Err(manox_ahp_runtime::error::RuntimeError::new(format!("unknown model: {}", m.0))
                            .with_code(manox_ahp_runtime::error::codes::MODEL_UNRESOLVABLE));
                    }
                }
            }
        };
        let approval = match intent.approval_mode.as_deref() {
            None => None,
            Some(s) => match serde_json::from_value::<PermissionMode>(Value::String(s.to_string()))
            {
                Ok(mode) => Some(mode),
                Err(_) => {
                    return Err(manox_ahp_runtime::error::RuntimeError::new(format!("unknown approval mode: {s}"))
                        .with_code(manox_ahp_runtime::error::codes::GATEWAY_BAD_REQUEST));
                }
            },
        };
        let effort = match intent.reasoning_effort.as_deref() {
            None => None,
            Some("high") => Some(ReasoningEffort::High),
            Some("max") => Some(ReasoningEffort::Max),
            Some(other) => {
                return Err(
                    manox_ahp_runtime::error::RuntimeError::new(format!("unknown reasoning effort: {other}"))
                        .with_code(manox_ahp_runtime::error::codes::GATEWAY_BAD_REQUEST),
                );
            }
        };
        // Seed blocks are kernel content blocks: validate the vocabulary
        // before anything is created, so a malformed seed cannot leave a
        // half-seeded session behind.
        let seed_blocks = match intent.seed.as_deref() {
            None | Some([]) => Vec::new(),
            Some(blocks) => {
                let mut parsed = Vec::with_capacity(blocks.len());
                for (i, block) in blocks.iter().enumerate() {
                    match serde_json::from_value::<manox_harness::types::ContentBlock>(
                        block.clone(),
                    ) {
                        Ok(b) => parsed.push(b),
                        Err(e) => {
                            return Err(manox_ahp_runtime::error::RuntimeError::new(
                                format!("seed block {i} is not a valid content block: {e}"),
                            )
                            .with_code(manox_ahp_runtime::error::codes::GATEWAY_BAD_REQUEST));
                        }
                    }
                }
                parsed
            }
        };
        // Idempotent re-open of a live session (§D.2).
        if let Some(existing) = intent.session_id.as_deref()
            && inner.sessions.lock().contains_key(existing)
        {
            inner.add_owner(existing, owner);
            return Ok(json!({ "session_id": existing }));
        }
        // §D.2 idempotency on disk (GW11): an id whose journal file already
        // exists is an existing COLD session — restore it through the
        // OpenSession path instead of minting a fresh session over the id.
        // The pre-fix fall-through reached `new_fresh` ("never restores the
        // previous session"), whose deferred materialization rewrote the
        // existing file wholesale on the first assistant message, erasing
        // the cold session's history. The probe reads the canonical
        // sessions-dir path directly rather than the store's scan-populated
        // map, so it holds even when no list refresh has ever run;
        // `note_session_path` seeds the identity map for the restore's
        // `load_thread`.
        if let Some(existing) = intent.session_id.as_deref()
            && let Some(path) = manox_ahp_runtime::paths::persisted_session_file(existing)
            && path.exists()
        {
            manox_agent::thread_store::global().with_mut(|s| s.note_session_path(existing, &path));
            open_session(inner, owner, existing).await?;
            return Ok(json!({ "session_id": existing }));
        }
        // Hidden-context seeding (`CreateSession.seed`): a seeded create
        // materializes the journal synchronously — header plus one
        // non-displaying `embedder_seed` custom row per block — and then
        // opens through the cold-restore path, so the seeds are durable
        // BEFORE the response returns (stronger than queueing appends on
        // the engine actor, and independent of its boot). An unseeded
        // create keeps the deferred-fresh flow untouched.
        if !seed_blocks.is_empty() {
            let session_id = uuid::Uuid::new_v4().to_string();
            let Some(path) = manox_ahp_runtime::paths::persisted_session_file(&session_id) else {
                return Err(manox_ahp_runtime::error::RuntimeError::new("minted session id failed the path gate")
                    .with_code(manox_ahp_runtime::error::codes::GATEWAY_INTERNAL));
            };
            let cwd = intent
                .cwd
                .clone()
                .map(PathBuf::from)
                .or_else(|| intent.project.clone().map(PathBuf::from))
                .unwrap_or_else(|| inner.cwd.clone());
            let storage = JsonlSessionStorage::create(
                &path,
                JsonlSessionMetadata {
                    id: session_id.clone(),
                    cwd: cwd.to_string_lossy().into_owned(),
                    created_at: chrono::Utc::now(),
                    parent_session_path: None,
                    metadata: Some(serde_json::json!({
                        "host": manox_agent::host::current().slug(),
                        "thread": session_id,
                    })),
                },
            )
            .await
            .map_err(|err| {
                manox_ahp_runtime::error::RuntimeError::new(format!("seeded session file creation failed: {err}"))
                    .with_code(manox_ahp_runtime::error::codes::GATEWAY_INTERNAL)
            })?;
            let seed_rows: Vec<manox_harness::session::SessionTreeEntry> = seed_blocks
                .into_iter()
                .scan(None::<String>, |parent, block| {
                    let entry_id = uuid::Uuid::new_v4().to_string();
                    let entry = manox_harness::session::SessionTreeEntry::CustomMessage {
                        id: entry_id.clone(),
                        parent_id: parent.clone(),
                        timestamp: chrono::Utc::now(),
                        custom_type: "embedder_seed".into(),
                        content: vec![block],
                        details: None,
                        display: false,
                    };
                    *parent = Some(entry_id);
                    Some(entry)
                })
                .collect();
            storage.append_entries(&seed_rows).await.map_err(|err| {
                manox_ahp_runtime::error::RuntimeError::new(format!("seed row append failed: {err}"))
                    .with_code(manox_ahp_runtime::error::codes::GATEWAY_INTERNAL)
            })?;
            drop(storage);
            // Path note + open are two steps, deliberately (review #778):
            // the note only seeds the store's id→path map so `load_thread`
            // can resolve the just-minted uuid before any list scan ran.
            // A concurrent open of the SAME id would need to guess the
            // fresh uuid (2^122 collision space) while holding this
            // connection's FIFO dispatch — the uuid, not a lock, is the
            // identity claim here, matching the deferred-fresh path.
            manox_agent::thread_store::global()
                .with_mut(|store| store.note_session_path(&session_id, &path));
            open_session(inner, owner, &session_id).await?;
            // Explicit intent overrides on the opened thread (durable change
            // rows); absent fields keep what the seeded journal restores.
            let thread = inner
                .sessions
                .lock()
                .get(&session_id)
                .map(|entry| entry.thread.clone());
            if let Some(thread) = thread {
                // Multi-root grants on the restored engine (the cold-open
                // fence started cwd-only; multi-working-dirs).
                for dir in &intent.working_directories {
                    thread.with_mut(|t| t.grant_working_directory(PathBuf::from(dir)));
                }
                thread.with_mut(|t| {
                    if let Some(model) = model {
                        t.set_model(model);
                    }
                    if let Some(mode) = approval {
                        t.set_permission_mode(mode);
                    }
                    if let Some(effort) = effort {
                        t.set_reasoning_effort(effort);
                    }
                    if let Some(project) = &intent.project {
                        t.bind_at_creation(PathBuf::from(project));
                    }
                });
            }
            if !intent.working_directories.is_empty() {
                let dirs = intent.working_directories.clone();
                manox_agent::thread_store::global()
                    .with_mut(|s| s.set_working_directories(&session_id, dirs));
            }
            return Ok(json!({ "session_id": session_id }));
        }

        let session_id = intent
            .session_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let project = intent.project.as_ref().map(PathBuf::from);
        let cwd = intent
            .cwd
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| project.clone().unwrap_or_else(|| inner.cwd.clone()));
        let thread = match &project {
            Some(p) => Thread::new_in_project(ThreadId(session_id.clone()), p.clone()),
            None => Thread::new_fresh(ThreadId(session_id.clone()), cwd),
        };
        // Multi-root: grant the extra working directories before the engine
        // materializes, so the gate's granted-root set admits them from the
        // first tool call (multi-working-dirs).
        for dir in &intent.working_directories {
            thread.with_mut(|t| t.grant_working_directory(PathBuf::from(dir)));
        }
        // Persist the grants (multi-working-dirs): the sidecar keeps them
        // for a cold restore. The path note seeds the id→path map first
        // (the journal materializes lazily at the first turn) so the
        // sidecar write is addressable.
        if !intent.working_directories.is_empty()
            && let Some(path) = manox_ahp_runtime::paths::persisted_session_file(&session_id)
        {
            let dirs = intent.working_directories.clone();
            manox_agent::thread_store::global().with_mut(|s| {
                s.note_session_path(&session_id, &path);
                s.set_working_directories(&session_id, dirs);
            });
        }
        // Intent application: model (explicit canonical or the global
        // default), approval mode, reasoning effort.
        let initial = model.or_else(manox_agent::provider_glue::default_model);
        thread.with_mut(|t| {
            if let Some(model) = initial {
                t.set_model(model);
            }
            if let Some(mode) = approval {
                t.set_permission_mode(mode);
            }
            if let Some(effort) = effort {
                t.set_reasoning_effort(effort);
            }
        });
        let turn_active = Arc::new(AtomicBool::new(false));
        let pending_submits = Arc::new(Mutex::new(Vec::new()));
        // The insert is a single-lock recheck: a racing same-id create/open
        // that won the race since the live check above has its entry adopted
        // and ours dropped — never clobbered, so the loser's intent seeds
        // cannot overwrite the winner's model / approval / effort.
        let loser = inner.insert_session_if_absent(
            session_id.clone(),
            ServerSession {
                thread: thread.clone(),
                turn_active,
                pending_submits,
            },
        );
        if loser.is_some() {
            tracing::warn!(
                session_id,
                "create lost a same-id race; adopting the winning entry"
            );
        }
        inner.add_owner(&session_id, owner);
        Ok(json!({ "session_id": session_id }))
    }

    /// Dispose one client's ownership of a session.
    ///
    /// Only the REQUESTING client loses ownership: the session survives for
    /// every other owner, and the entry (with its `ThreadHandle`) leaves only
    /// when the last owner does. A running turn is cancelled at that point —
    /// there is nobody left to settle it.
    pub(crate) fn dispose_session(&self, owner: &str, session_id: &str) {
        self.remove_owner(owner, session_id);
        if !self.owners(session_id).is_empty() {
            return;
        }
        let removed = self.sessions.lock().remove(session_id);
        if let Some(session) = removed {
            self.clear_embedder_tools(session_id);
            if session.turn_active.load(Ordering::SeqCst) {
                session.thread.with_mut(|t| t.cancel());
                manox_agent::thread_store::global().with_mut(|s| s.mark_idle(session_id));
            }
        }
    }

    pub(crate) async fn submit(
        self: &Arc<Self>,
        owner: &str,
        session_id: &str,
        text: String,
        images: Vec<ImageAttachment>,
        client_id: Option<String>,
        origin_rpc: Option<String>,
    ) -> Result<Value, manox_ahp_runtime::error::RuntimeError> {
        // Bind redirect + restart window: submissions addressed to a
        // superseded predecessor land on the successor, opened under its
        // own id with this connection as owner. Only a real redirect
        // auto-opens — a cold id keeps not-found semantics (review #805
        // r2 [issue] B).
        let effective = self.resolve_redirect(session_id);
        if effective != session_id && !self.sessions.lock().contains_key(&effective) {
            let _ = open_session(self, owner, &effective).await;
        }
        let session_id = effective.as_str();
        let receipt = |accepted: bool, message_id: Option<String>| {
            Ok(json!({ "accepted": accepted, "message_id": message_id }))
        };
        let images: Vec<(String, String)> = images
            .into_iter()
            .map(|i| (base64_bytes::encode(&i.data), i.mime_type))
            .collect();
        let slash = if images.is_empty() {
            parse_slash(&text)
        } else {
            None
        };
        // Navigation built-ins take effect immediately even mid-turn.
        if let Some((name, _)) = slash.as_ref()
            && let Some(builtin) = manox_agent::slash_builtins::canonical_builtin(name)
            && matches!(builtin.name, "exit" | "new")
        {
            self.archive_thread(owner, session_id, true);
            return receipt(true, None);
        }
        let Some(session) = self.sessions.lock().get(session_id).map(|s| {
            (
                s.thread.clone(),
                s.turn_active.clone(),
                s.pending_submits.clone(),
            )
        }) else {
            return Err(unknown_session(session_id));
        };
        let (thread, turn_active, pending_submits) = session;
        let client_id = client_id.unwrap_or_else(|| owner.to_string());
        if turn_active.load(Ordering::SeqCst) && slash.is_none() {
            let ui = thread.read(|t| MessageUiMetadata {
                model_id: t.model().map(|m| m.id.clone()),
                approval_mode: Some(t.permission_mode().as_i64()),
                ..Default::default()
            });
            pending_submits.lock().push(QueuedSubmit {
                client_id,
                text,
                images,
                ui,
                origin: origin_rpc,
            });
            return receipt(true, None);
        }
        // Slash resolution first (its handlers mutate the facade): a slash
        // hit keeps the legacy transcript semantics and never takes the
        // accept-time persist path. The origin pin lands here, as before —
        // a run started by a slash builtin carries it.
        let slashed = thread.with_mut(|t| {
            t.set_pending_turn_origin(origin_rpc.clone());
            if let Some((name, args)) = slash {
                let ui = MessageUiMetadata {
                    model_id: t.model().map(|m| m.id.clone()),
                    approval_mode: Some(t.permission_mode().as_i64()),
                    ..Default::default()
                };
                let slash_ui = MessageUiMetadata {
                    display_text: Some(text.clone()),
                    ..ui
                };
                let builtin_hit = t.run_slash_builtin(&name, &args, Some(slash_ui.clone()));
                let command_hit = manox_agent::command::try_global().is_some()
                    && t.submit_command(&name, &args, Some(slash_ui.clone()));
                let skill_hit = manox_agent::skill::try_global().is_some()
                    && t.submit_skill(&name, &args, Some(slash_ui));
                return builtin_hit || command_hit || skill_hit;
            }
            false
        });
        if slashed {
            return receipt(true, None);
        }
        if text.trim().is_empty() && images.is_empty() {
            return receipt(false, None);
        }
        // U6b②/④: the implicit dismissal — a Submit landing while a plan
        // review is pending means the user is discussing or revising, not
        // accepting (the desktop consumes its card locally; the durable
        // half lives HERE, where the session facade has the engine that
        // journals the `resolved` plan_review edge). Both planes clear:
        // the session facade (engine cmd → journal + sidecar) and the
        // store badge flag; this turn's own §D.5 deltas then carry the
        // cleared flag to every client.
        if manox_agent::thread_store::try_global()
            .map(|store| store.read(|s| s.pending_plan_contains(session_id)))
            .unwrap_or(false)
        {
            if let Some(thread) = self.session_thread(session_id) {
                thread.with_mut(|t| t.set_plan_review_pending(false));
            }
            if let Some(store) = manox_agent::thread_store::try_global() {
                store.with_mut(|s| s.mark_pending_plan(session_id, false));
            }
        }
        // K5 (accepted ⟹ logged): persist the user entry BEFORE the
        // receipt. Skipped when residual pending prompts would make
        // run_turn's merged prompt differ from this text — the actor's
        // drain-time persistence then covers the merged entry under the
        // same content-match contract (persist_prompt_user_entry). An
        // unmaterialized engine answers Ok(None) and falls to the same
        // drain path.
        let mut accepted_entry: Option<String> = None;
        if !thread.read(|t| t.has_pending_prompts())
            && let Some(engine) = thread.read(|t| t.engine_handle())
        {
            // The SAME (text, images) run_turn will hand the engine — the
            // middleware skip is an exact content match (K5 contract):
            // text normalized like `to_message_content` (no Text block
            // when blank), images as kernel ContentBlocks. K5 edge: the
            // engine expands before persisting, so the entry (and the pin)
            // carry the POST-expansion shape the run announces.
            let persist_text = if text.trim().is_empty() {
                String::new()
            } else {
                text.clone()
            };
            let blocks: Vec<manox_harness::types::ContentBlock> = images
                .iter()
                .map(
                    |(data, mime_type)| manox_harness::types::ContentBlock::Image {
                        data: data.clone(),
                        mime_type: mime_type.clone(),
                    },
                )
                .collect();
            match engine
                .persist_user_submission(&persist_text, blocks, origin_rpc.clone())
                .await
            {
                Ok(id) => accepted_entry = id,
                Err(err) => {
                    // No durability, no receipt. Un-pin the origin so a
                    // later run does not misattribute this refused submit,
                    // and tell the client why.
                    thread.with_mut(|t| t.set_pending_turn_origin(None));
                    let message = format!("submit persistence failed: {err}");
                    return Err(manox_ahp_runtime::error::RuntimeError::new(message)
                        .with_code(manox_ahp_runtime::error::codes::GATEWAY_INTERNAL));
                }
            }
        }
        // Insert + run. The pin arms the actor's middleware skip so the
        // run's own user MessageEnd records the accepted entry instead of
        // appending a duplicate.
        thread.with_mut(|t| {
            t.set_pending_turn_accepted_entry(accepted_entry);
            let ui = MessageUiMetadata {
                model_id: t.model().map(|m| m.id.clone()),
                approval_mode: Some(t.permission_mode().as_i64()),
                ..Default::default()
            };
            let content = to_message_content(text, images);
            t.insert_user_message_with_content_and_ui_metadata(content, Some(ui));
            t.run_turn();
        });
        let message_id = thread.read(|t| t.last_user_message_id().map(str::to_string));
        receipt(true, message_id)
    }

    /// §D.2 `Steer`: injects the steer and answers with the receipt
    /// `{accepted, message_id?}` (the echo of the call's steer id). The
    /// compat `ClientNote::Steer` forwards here with its `client_id` as
    /// `message_id`.
    pub(crate) fn steer(
        &self,
        session_id: &str,
        message_id: String,
        text: String,
        images: Vec<ImageAttachment>,
        // The steer id IS the echo correlation (the client retires its
        // echo when the steer's own injection settles); no origin pin.
        _origin_rpc: Option<String>,
    ) -> Result<Value, manox_ahp_runtime::error::RuntimeError> {
        let images: Vec<(String, String)> = images
            .into_iter()
            .map(|i| (base64_bytes::encode(&i.data), i.mime_type))
            .collect();
        let Some((thread, pending_submits)) = self
            .sessions
            .lock()
            .get(session_id)
            .map(|s| (s.thread.clone(), s.pending_submits.clone()))
        else {
            return Err(unknown_session(session_id));
        };
        // A steer removes its own parked follow-up so the turn-end drain does
        // not resend the same text as a plain follow-up.
        pending_submits.lock().retain(|q| q.client_id != message_id);
        thread.with_mut(|t| {
            let ui = MessageUiMetadata {
                model_id: t.model().map(|m| m.id.clone()),
                approval_mode: Some(t.permission_mode().as_i64()),
                ..Default::default()
            };
            let content = to_message_content(text, images);
            if t.is_running() {
                // S3 stable-id: thread the client's `Steer` message id through
                // the facade so the optimistic bubble, the injected `user`
                // journal row, and the echo retirement share one identity.
                t.enqueue_steer(content, Some(ui), Some(message_id.clone()));
            } else {
                t.insert_user_message_with_content_and_ui_metadata(content, Some(ui));
                t.run_turn();
            }
        });
        // T10 (§D.6): the `SteerPending` note mirror is gone — the steer's
        // durable identity is the `message` journal row (`originRpc` echo
        // retirement for the submitting client; every owner sees the row on
        // the follow stream).
        Ok(json!({
            "accepted": true,
            "message_id": message_id,
        }))
    }

    pub(crate) fn drop_queued(&self, session_id: &str, client_id: String) {
        let resolved = self.resolve_redirect(session_id);
        if let Some(session) = self.sessions.lock().get(&resolved) {
            let pending = session.pending_submits.clone();
            pending.lock().retain(|q| q.client_id != client_id);
        }
    }

    /// Select the session's model.
    ///
    /// The refusal is reported to the caller, not only noted to the v2 clients:
    /// the AHP dispatch path has already folded the change into the state its
    /// subscribers reduce, so a swallowed failure would let every client
    /// converge on a model the session is not running.
    pub(crate) fn set_model(
        &self,
        session_id: &str,
        id: &str,
    ) -> Result<(), manox_ahp_runtime::error::RuntimeError> {
        let Some(thread) = self.session_thread(session_id) else {
            return Err(unknown_session(session_id));
        };
        let registry = manox_agent::provider_glue::global();
        match manox_harness::model_ref::resolve_model_ref(&registry, id) {
            Some(model) => {
                // T10: the v1 `ThreadInfo` republish is gone — the engine
                // journals the change and the P-face delta refreshes chips.
                thread.with_mut(|t| t.set_model(model));
                Ok(())
            }
            None => Err(unresolvable_model(id)),
        }
    }

    pub(crate) fn set_reasoning_effort(
        &self,
        session_id: &str,
        effort: &str,
    ) -> Result<(), manox_ahp_runtime::error::RuntimeError> {
        let Some(thread) = self.session_thread(session_id) else {
            return Err(unknown_session(session_id));
        };
        let effort = match effort {
            "high" => ReasoningEffort::High,
            "max" => ReasoningEffort::Max,
            _ => {
                return Err(manox_ahp_runtime::error::RuntimeError::new(
                    "set_reasoning_effort requires effort: high|max",
                )
                .with_code(manox_ahp_runtime::error::codes::GATEWAY_BAD_REQUEST));
            }
        };
        thread.with_mut(|t| t.set_reasoning_effort(effort));
        Ok(())
    }

    pub(crate) fn set_approval_mode(
        &self,
        session_id: &str,
        mode: &str,
    ) -> Result<(), manox_ahp_runtime::error::RuntimeError> {
        let Some(thread) = self.session_thread(session_id) else {
            return Err(unknown_session(session_id));
        };
        // Parse strictly: an unparseable mode must not settle on the default
        // (a silent no-op beats a chip bounce-back on a projected mutation).
        let Ok(mode) = serde_json::from_value::<PermissionMode>(Value::String(mode.to_string()))
        else {
            return Err(manox_ahp_runtime::error::RuntimeError::new(format!(
                "unknown approval mode: {mode}"
            ))
            .with_code(manox_ahp_runtime::error::codes::GATEWAY_BAD_REQUEST));
        };
        thread.with_mut(|t| t.set_permission_mode(mode));
        Ok(())
    }

    pub(crate) async fn set_cwd(
        self: &Arc<Self>,
        session_id: &str,
        cwd: &str,
    ) -> Result<(), manox_ahp_runtime::error::RuntimeError> {
        // Restart-window opens from a note carry no connection identity:
        // use a transient owner and release it right after so it never
        // defeats the orphaned-session reap (review #805 [sugg] 11).
        const NOTE_OWNER: &str = "note-setcwd";
        let effective = self.resolve_redirect(session_id);
        // Only a real redirect auto-opens here (a cold id keeps not-found
        // semantics, review #805 r2 [issue] B); the transient owner stays
        // until the hand-off releases it so the session is never ownerless
        // (r2 [sugg] G).
        if effective != session_id && !self.sessions.lock().contains_key(&effective) {
            let _ = open_session(self, NOTE_OWNER, &effective).await;
        }
        // The transient owner rides the OPENED id (never the id a bind
        // may mint later), and every exit path releases it (review r3
        // [problem] 2).
        let opened_id = (effective != session_id && self.sessions.lock().contains_key(&effective))
            .then(|| effective.clone());
        let Some(thread) = self.session_thread(&effective) else {
            if let Some(id) = &opened_id {
                self.remove_owner(NOTE_OWNER, id);
            }
            return Err(unknown_session(session_id));
        };
        if thread.read(|t| t.has_interacted()) {
            // The working-directory switch applies at ANY interaction
            // state, through the same per-call cwd machinery the model's
            // tools use: sticky advance + a durable `cwd_change` entry —
            // never the header cwd, never a re-bind.
            thread.with_mut(|t| t.set_cwd(cwd.into()));
            if let Some(id) = &opened_id {
                self.remove_owner(NOTE_OWNER, id);
            }
            return Ok(());
        }
        // Bind on a not-yet-interacted thread: identity follows the log —
        // the directory becomes a SUCCESSOR session (fresh chain bound at
        // creation), and this predecessor degrades into a redirect stub
        // sharing the successor's engine (#802: no chain swap under a
        // live identity, ever).
        let inner = Arc::clone(self);
        let session_id = session_id.to_string();
        let cwd = cwd.to_string();
        manox_agent::runtime::handle().spawn(async move {
            if let Err(error) = inner.bind_successor(&session_id, &cwd).await {
                tracing::warn!(session = %session_id, %error, "bind successor failed");
            }
            // Released AFTER the hand-off, on the id the owner was added
            // to (review r3 [problem] 2).
            if let Some(id) = opened_id {
                inner.remove_owner(NOTE_OWNER, &id);
            }
        });
        Ok(())
    }

    /// The bind hand-off (#802 / identity-follows-log): mint the successor
    /// session bound to `cwd`, mark the predecessor superseded (sidecar +
    /// live map), stub the predecessor entity onto the successor's engine
    /// so live streams/ops addressed to it converge on the new log, and
    /// publish the control-face hand-off (`SessionDisposed { successor }`).
    async fn bind_successor(self: &Arc<Self>, pred_id: &str, cwd: &str) -> Result<(), String> {
        // Dedupe keys on the RESOLVED id: a note may arrive on any
        // predecessor of the same redirect chain, and inserting one id
        // while removing another wedges the set forever (review #805 r2
        // [issue] A).
        let key = self.resolve_redirect(pred_id);
        if !self.binding.lock().insert(key.clone()) {
            return Err("bind already in flight".into());
        }
        let outcome = self.bind_successor_impl(pred_id, cwd).await;
        self.binding.lock().remove(&key);
        outcome
    }

    async fn bind_successor_impl(self: &Arc<Self>, pred_id: &str, cwd: &str) -> Result<(), String> {
        // The lease is armed as soon as the successor exists and releases
        // the internal owner on EVERY exit — including the narrow failure
        // window after `create` (review r3 [sugg] 3).
        let pred = self.session_thread(pred_id).ok_or("unknown session")?;
        let (model, approval, effort) = pred.read(|t| {
            (
                t.model()
                    .map(|m| manox_journal::ModelRef::new(format!("{}/{}", m.provider, m.id))),
                t.permission_mode().wire().to_string(),
                match t.reasoning_effort() {
                    manox_agent::language_model::ReasoningEffort::High => "high",
                    manox_agent::language_model::ReasoningEffort::Max => "max",
                }
                .to_string(),
            )
        });
        let created = Self::create_session_request(
            self,
            "server-bind",
            SessionIntent {
                session_id: None,
                cwd: Some(cwd.to_string()),
                project: Some(cwd.to_string()),
                initial_model: model,
                approval_mode: Some(approval),
                reasoning_effort: Some(effort),
                seed: None,
                working_directories: Vec::new(),
            },
        )
        .await
        .map_err(|e| e.message)?;
        let succ_id = created["session_id"]
            .as_str()
            .ok_or("create answered without a session id")?
            .to_string();
        let _bind_lease = OwnerLease {
            inner: Arc::clone(self),
            owner: "server-bind",
            id: succ_id.clone(),
        };
        // Every fail-able step runs BEFORE the durable hand-off: past
        // `mark_superseded` there is no rollback, so a failure below this
        // line would strand a superseded predecessor with no alias (review
        // #805 r2 [sugg] F).
        let succ = self
            .session_thread(&succ_id)
            .ok_or("successor session missing after create")?;
        let engine = succ
            .read(|t| t.engine_clone())
            .ok_or("successor engine not materialized")?;
        // The hand-off audience: every client owning the id the note
        // arrived on OR the id it resolved to — they differ once a
        // predecessor has already handed off, whose own owner set is
        // cleared by that earlier hand-off (plan §3.1 B-iv).
        let effective = self.resolve_redirect(pred_id);
        let mut audience: Vec<String> = self.owners(pred_id);
        for owner in self.owners(&effective) {
            if !audience.contains(&owner) {
                audience.push(owner);
            }
        }
        // Owner inheritance: the successor takes the predecessor's audience,
        // so a client watching the predecessor keeps watching the session
        // across the hand-off.
        for owner in &audience {
            // `add_owner(session_id, client_id)`: the successor takes the
            // audience, not the other way around.
            self.add_owner(&succ_id, owner);
        }
        // Stub the predecessor onto the successor's engine + mirrored header
        // fields: its journal seam answers the successor's log from here on.
        pred.with_mut(|t| t.adopt_successor(engine, PathBuf::from(cwd)));
        // Both ids mark the successor: the id clients still address, and the
        // id it resolved to.
        manox_agent::thread_store::global().with_mut(|s| {
            s.mark_superseded(pred_id, &succ_id);
            if effective != pred_id {
                s.mark_superseded(&effective, &succ_id);
            }
        });
        self.superseded
            .lock()
            .insert(pred_id.to_string(), succ_id.clone());
        self.superseded
            .lock()
            .insert(effective.clone(), succ_id.clone());
        // The bound directory becomes (or joins) a workspace row, and the
        // successor leads its account. A workspace-store failure is logged,
        // not fatal: the session hand-off is the user's intent and the
        // bookkeeping row is not worth failing it over.
        match manox_workspace::WorkspaceStore::open() {
            Ok(store) => match store.create(std::path::Path::new(cwd)) {
                Ok((view, _)) => {
                    if let Err(error) = store.attach_session(&view.workspace_id, &succ_id) {
                        tracing::debug!(%error, "workspace attach after bind skipped");
                    }
                }
                Err(error) => tracing::debug!(%error, "workspace create after bind skipped"),
            },
            Err(error) => tracing::debug!(%error, "workspace store unavailable after bind"),
        }
        // The hand-off is complete: the audience is inherited by the
        // successor, so both predecessor ids drop their ownership here.
        // Without this an entry could never satisfy the reap predicate while
        // the client stays connected.
        for owner in &audience {
            self.remove_owner(owner, pred_id);
            if effective != pred_id {
                self.remove_owner(owner, &effective);
            }
        }
        self.reap_superseded_if_idle(pred_id);
        if effective != pred_id {
            self.reap_superseded_if_idle(&effective);
        }
        // No explicit owner release here: `OwnerLease` covers the bind
        // owner on every exit path, and the note-open owner belongs to
        // `set_cwd`'s own scope (review r3 [problem] 2, [sugg] 3).
        Ok(())
    }

    /// Drop a superseded predecessor's entry once nothing consumes it: without
    /// this it stays resident for the process lifetime. A running turn keeps
    /// its entry — there is still a settle path to reach.
    fn reap_superseded_if_idle(&self, session_id: &str) {
        if !self.superseded.lock().contains_key(session_id) {
            return;
        }
        if !self.owners(session_id).is_empty() {
            return;
        }
        // Check-and-remove under ONE hold: a turn turning active between two
        // holds would otherwise be reaped mid-flight with nobody left to
        // settle it.
        let removed = {
            let mut sessions = self.sessions.lock();
            let running = sessions
                .get(session_id)
                .is_some_and(|s| s.turn_active.load(Ordering::SeqCst));
            if running {
                return;
            }
            sessions.remove(session_id)
        };
        if removed.is_some() {
            // The registrations die with the entry, like every other removal
            // path.
            self.clear_embedder_tools(session_id);
            tracing::debug!(session = %session_id, "reaped superseded predecessor");
        }
    }

    pub(crate) fn compact(&self, session_id: &str, instructions: Option<String>) -> Result<(), manox_ahp_runtime::error::RuntimeError> {
        let Some(thread) = self.session_thread(session_id) else {
            return Err(unknown_session(session_id));
        };
        thread.with_mut(|t| t.compact(instructions));
        Ok(())
    }

    pub(crate) fn plan_seed(&self, session_id: &str, plan_file: &str) -> Result<(), manox_ahp_runtime::error::RuntimeError> {
        let Some(thread) = self.session_thread(session_id) else {
            return Err(unknown_session(session_id));
        };
        let plan_file = plan_file.to_string();
        let seed_text = match manox_agent::collaboration_mode::render_plan_mode_approved(&plan_file)
        {
            Ok(text) => text,
            Err(error) => {
                return Err(manox_ahp_runtime::error::RuntimeError::new(
                    error.to_string(),
                )
                .with_code(manox_ahp_runtime::error::codes::GATEWAY_BAD_REQUEST));
            }
        };
        thread.with_mut(|t| {
            let ui = MessageUiMetadata {
                model_id: t.model().map(|m| m.id.clone()),
                approval_mode: Some(t.permission_mode().as_i64()),
                // The expanded plan directive is written by the harness on
                // the user's behalf, so the bubble header names the harness
                // (the desktop's local seed path always did; U6b③ aligns
                // the gateway path as both migrate onto it).
                author: Some(manox_agent::MessageAuthor::Harness),
                ..Default::default()
            };
            t.seed_plan_execution(plan_file, seed_text, Some(ui));
        });
        Ok(())
    }

    /// Apply one goal lifecycle action (the `x-manox/goal` seam).
    ///
    /// The engine owns every rule — an unknown action is not silently a no-op
    /// here, because the caller has already folded the command into its
    /// declaration surface; a `Refused` would be a promise the runtime broke.
    pub(crate) fn goal(
        &self,
        session_id: &str,
        action: &str,
        objective: Option<String>,
        budget: Option<u64>,
        max_rounds: Option<u64>,
    ) -> Result<(), manox_ahp_runtime::error::RuntimeError> {
        // Validate the verb before touching state: an unknown action is the
        // caller's mistake, and reporting it as a thread-level failure would
        // point at the wrong thing.
        if !matches!(action, "create" | "edit" | "replace" | "clear" | "pause" | "resume") {
            return Err(manox_ahp_runtime::error::RuntimeError::new(format!(
                "unknown goal action: {action}"
            ))
            .with_code(manox_ahp_runtime::error::codes::GATEWAY_BAD_REQUEST));
        }
        let Some(thread) = self.session_thread(session_id) else {
            return Err(unknown_session(session_id));
        };
        let objective = objective.unwrap_or_default();
        let actor = manox_agent::db::GoalActor::User;
        let result = thread.with_mut(|t| match action {
            "create" => t.set_goal(objective),
            "edit" => t.edit_goal(objective, budget, max_rounds, actor),
            "replace" => t.replace_goal(objective, budget, max_rounds, actor),
            "clear" => t.clear_goal(actor),
            "pause" => t.set_goal_status(
                manox_agent::goal::GoalStatus::Paused,
                Some(manox_agent::goal::GoalBlockReason {
                    code: "user-paused".into(),
                    message: "paused by user".into(),
                }),
                actor,
            ),
            "resume" => t.set_goal_status(manox_agent::goal::GoalStatus::Active, None, actor),
            // Unreachable: the verb was validated above. Answering Ok here
            // rather than panicking keeps a future verb from becoming a crash.
            _ => Ok(()),
        });
        result.map_err(|error| {
            manox_ahp_runtime::error::RuntimeError::new(error.to_string())
                .with_code(manox_ahp_runtime::error::codes::GATEWAY_BAD_REQUEST)
        })
    }

    pub(crate) fn archive_thread(&self, owner: &str, session_id: &str, archived: bool) {
        if archived {
            // K3 (delivery request): journal the archive decision BEFORE
            // the dispose — while the engine route is still alive the
            // pinned_archived row rides the actor's serializer (K4
            // fail-loud) instead of racing the retire protocol's
            // cold-append fallback. The pump is also still alive, so
            // follow streams deliver the entry before the dispose closes
            // them.
            manox_agent::thread_store::global().with_mut(|s| s.archive_thread(session_id, true));
            self.dispose_session(owner, session_id);
        } else {
            manox_agent::thread_store::global().with_mut(|s| s.archive_thread(session_id, false));
        }
    }

    /// Pin or unpin a session (the AHP extension surface: AHP has no pin bit).
    ///
    /// The durable authority is the store row plus the session's sidecar, the
    /// same path v2's `PinThread` takes — a pin the sidebar shows and a cold
    /// restore forgets would be the worst of both.
    pub(crate) fn pin_session(&self, session_id: &str, pinned: bool) -> bool {
        let mut applied = false;
        manox_agent::thread_store::global().with_mut(|store| {
            applied = store.summary_by_id(session_id).is_some();
            if applied {
                store.pin_thread(session_id, pinned);
            }
        });
        applied
    }

    /// Move a session before another in the sidebar order, or to the head when
    /// `before` is `None`.
    ///
    /// A move that names an unaccounted row changes nothing and reports it: the
    /// store's own `MoveInvalid` is the answer, surfaced rather than folded into
    /// a success the sidebar would not show.
    pub(crate) fn order_session(&self, session_id: &str, before: Option<&str>) -> bool {
        // The store's move is fire-and-forget by design (it warns and leaves the
        // account untouched on an unaccounted row or anchor), so what the caller
        // can honestly be told is whether the row exists at all — an unknown
        // session is a client error, while a rejected *move* is the store's own
        // accounting to log.
        let known = manox_agent::thread_store::global()
            .read(|store| store.summary_by_id(session_id).is_some());
        if !known {
            return false;
        }
        manox_agent::thread_store::global()
            .with_mut(|store| store.insert_thread_before(session_id, before));
        true
    }

    /// Rename a session to a user-supplied title.
    ///
    /// Two calls, and the split matters:
    ///
    /// - `handle_notice` delivers the live [`manox_agent::thread::ThreadEvent::TitleChanged`]
    ///   to the **facade**, so the sidebar and any in-process observer show the
    ///   new name at once.
    /// - `thread_store::rename_thread` performs the **durable** half: it appends
    ///   the journal `title` entry (the row the v2 projection folds and the AHP
    ///   translator reads) and writes the sidecar.
    ///
    /// The facade call deliberately does **not** stand in for persistence: it
    /// feeds the consumer end of the engine's notice chain, which is *past* its
    /// journal tap, so a notice injected here is never journalled. That is why
    /// the store owns the durable append (the `journal_pinned_archived`
    /// precedent) rather than this method reusing the engine's emission path.
    ///
    /// A whitespace-only title is refused: it would blank the session's name
    /// while looking like it landed.
    pub(crate) fn rename_thread(
        &self,
        session_id: &str,
        title: &str,
    ) -> manox_agent::thread_store::RenameOutcome {
        let title = title.trim();
        if title.is_empty() {
            return manox_agent::thread_store::RenameOutcome::Blank;
        }
        let Some(thread) = self.session_thread(session_id) else {
            return manox_agent::thread_store::RenameOutcome::UnknownSession;
        };
        // The live notice feeds the facade; the store call is the durable half
        // (journal row + sidecar). Its outcome is what the caller can honestly
        // report — a session another process is driving takes neither leg.
        thread.handle_notice(BackendNotice::Event(Box::new(
            manox_agent::thread::ThreadEvent::TitleChanged {
                title: title.to_string(),
            },
        )));
        manox_agent::thread_store::global().with_mut(|s| s.rename_thread(session_id, title))
    }
}

// ── Client capabilities (the engine's window into the frontend). ─────────────

/// Routes the engine's frontend capability calls to the AHP client that
/// declared them.
///
/// The capability belongs to whichever client is watching the session, and AHP
/// makes that a declaration (`serverRequests`) rather than a guess: the
/// candidate set is the session channel's subscribers filtered by their own
/// claim. There is no second transport to fall back to, so a session with no
/// declared owner for a capability is a **refusal** — the engine's fail-closed
/// contract depends on that being an error rather than a quiet no-op.
pub(crate) struct AgentServerCapabilityClient(Arc<AgentServerInner>);

impl AgentServerCapabilityClient {
    pub(crate) fn new(server: &AgentServer) -> Self {
        Self(Arc::clone(&server.0))
    }
}

/// Ask a session's AHP subscribers to perform `method`.
///
/// `Err` covers all three failure shapes — no AHP host, no session context, no
/// declared client, a client that failed — because the engine's contract is the
/// same for each: fail closed, never invent a reply.
async fn route_session_capability(method: &str, params: Value) -> Result<Value, String> {
    let session_id = manox_agent::capability::CURRENT_SESSION
        .try_with(|c| c.clone())
        .ok()
        .flatten()
        .ok_or_else(|| format!("no session context for {method}"))?;
    let runtime = manox_ahp_runtime::ahp::runtime::try_runtime()
        .ok_or_else(|| no_capable_client_error(method))?;
    if !runtime.has_capable_client(&session_id, method) {
        return Err(no_capable_client_error(method));
    }
    runtime
        .request_client(&session_id, method, params)
        .await
        .map_err(|error| error.message())
}

/// The refusal when no connected client can serve a capability call.
fn no_capable_client_error(method: &str) -> String {
    format!("no client can answer this capability call: {method}")
}

impl manox_agent::capability::CapabilityClient for AgentServerCapabilityClient {
    fn browser_op(
        &self,
        op: manox_agent::thread_engine::BrowserOp,
    ) -> futures::future::BoxFuture<'static, Result<manox_agent::thread_engine::BrowserReply, String>>
    {
        Box::pin(async move {
            let op_value = serde_json::to_value(&op).map_err(|e| e.to_string())?;
            let reply = route_session_capability(manox_ahp::ext::requests::BROWSER_OP, op_value)
                .await
                .map_err(|e| format!("browser op failed: {e}"))?;
            serde_json::from_value(reply).map_err(|e| format!("browser reply invalid: {e}"))
        })
    }

    fn clipboard_read(&self) -> futures::future::BoxFuture<'static, Result<Option<String>, String>> {
        Box::pin(async move {
            let reply = route_session_capability(manox_ahp::ext::requests::CLIPBOARD_READ, Value::Null)
                .await?;
            Ok(reply.as_str().map(str::to_string))
        })
    }

    fn open_external(&self, url: String) -> futures::future::BoxFuture<'static, Result<(), String>> {
        Box::pin(async move {
            route_session_capability(
                manox_ahp::ext::requests::OPEN_EXTERNAL,
                serde_json::json!({ "url": url }),
            )
            .await
            .map(|_| ())
        })
    }
}

/// Build kernel `MessageContent` from a submit/steer payload: text plus
/// base64-encoded image blocks.
fn to_message_content(text: String, images: Vec<(String, String)>) -> Vec<MessageContent> {
    let mut content = Vec::new();
    if !text.trim().is_empty() {
        content.push(MessageContent::Text(text));
    }
    content
        .into_iter()
        .chain(
            images
                .into_iter()
                .map(|(data, mime_type)| MessageContent::Image { data, mime_type }),
        )
        .collect()
}

/// Split a `/name args` invocation; an empty name is not a slash turn.
fn parse_slash(text: &str) -> Option<(String, String)> {
    let body = text.trim_start().strip_prefix('/')?;
    let (name, args) = body.split_once(char::is_whitespace).unwrap_or((body, ""));
    let name = name.trim();
    (!name.is_empty()).then(|| (name.to_string(), args.trim_start().to_string()))
}

// ── Embedder tool bridge (dspo/manox-app#11): RegisterSessionTools store +
// the engine-facing provider whose adapters round-trip executions to the
// registering client as InvokeClientTool. ─────────────────────────────────

/// The AgentServer's `EmbedderToolProvider` impl: hands the engine the
/// session's registered embedder tools at tool-assembly time. The engine
/// wraps each in the approval gate, exactly like MCP tools.
pub struct AgentServerEmbedderTools(Arc<AgentServerInner>);

impl AgentServerEmbedderTools {
    /// Wrap an `AgentServer` so the engine's tool assembly consults its
    /// registrations.
    pub fn new(server: &AgentServer) -> Self {
        Self(server.0.clone())
    }
}

impl manox_agent::embedder_tools::EmbedderToolProvider for AgentServerEmbedderTools {
    fn tools_for(
        &self,
        session_id: &str,
    ) -> Vec<std::sync::Arc<dyn manox_harness::tool::AgentTool>> {
        let registrations = self.0.embedder_tools.lock();
        let Some(clients) = registrations.get(session_id) else {
            return Vec::new();
        };
        clients
            .iter()
            .flat_map(|(client_id, tools)| {
                tools.iter().map(move |spec| {
                    std::sync::Arc::new(EmbedderToolAdapter {
                        inner: self.0.clone(),
                        session_id: session_id.to_string(),
                        client_id: client_id.clone(),
                        client_name: manox_agent::embedder_tools::client_tool_name(&spec.name),
                        spec: spec.clone(),
                    }) as std::sync::Arc<dyn manox_harness::tool::AgentTool>
                })
            })
            .collect()
    }
}

/// One registered embedder tool as the engine sees it: the schema is the
/// host's verbatim; execution routes to the registering client and the
/// reply's `{content, isError}` settles the call.
struct EmbedderToolAdapter {
    inner: Arc<AgentServerInner>,
    session_id: String,
    client_id: String,
    spec: ClientToolSpec,
    /// The sanitized model-facing name, computed once at construction —
    /// `name()` borrows it, so nothing leaks (review #779 replaced a
    /// `Box::leak` here; per-adapter Strings are bounded by the
    /// registration set and die with it).
    client_name: String,
}

#[async_trait::async_trait]
impl manox_harness::tool::AgentTool for EmbedderToolAdapter {
    fn name(&self) -> &str {
        &self.client_name
    }
    fn description(&self) -> &str {
        &self.spec.description
    }
    fn parameters_schema(&self) -> Value {
        self.spec.input_schema.clone()
    }
    /// An embedder tool is a remote call into the host — mutating by
    /// default, like MCP tools.
    /// The registrant's advisory hint; the approval gate wrapping this
    /// tool stays the authority (a read_only registration skips the
    /// approval card, a mutating one surfaces it).
    fn requires_approval(&self, _params: &Value) -> bool {
        !self.spec.read_only
    }
    /// Mirror of the registrant's read_only hint: the approval gate's
    /// `needs_gate` is `requires_approval() || !is_read_only()`, so without
    /// this override a read_only registration still fell through the gate
    /// and — outside danger-full-access — into its fail-closed deny arm.
    fn is_read_only(&self) -> bool {
        self.spec.read_only
    }
    async fn execute(
        &self,
        tool_call_id: &str,
        params: Value,
        _signal: tokio_util::sync::CancellationToken,
        _ctx: &dyn manox_harness::tool::ToolContext,
    ) -> Result<manox_harness::tool::AgentToolResult, manox_harness::tool::ToolError> {
        // The tool belongs to the client that registered it, and AHP's
        // declaration is what names that client, so the request is addressed
        // by capability rather than by the id captured at registration time
        // (which a reconnecting client would have replaced).
        let reply = route_session_capability(
            manox_ahp::ext::requests::INVOKE_TOOL,
            serde_json::json!({
                "clientId": self.client_id,
                "toolCallId": tool_call_id,
                "name": self.spec.name,
                "input": params,
            }),
        )
        .await
        .map_err(manox_harness::tool::ToolError::ExecutionFailed)?;
        let content = reply
            .get("content")
            .and_then(|c| c.as_str())
            .ok_or_else(|| {
                manox_harness::tool::ToolError::ExecutionFailed(
                    "client tool reply missing content".into(),
                )
            })?;
        if reply
            .get("isError")
            .and_then(|e| e.as_bool())
            .unwrap_or(false)
        {
            return Err(manox_harness::tool::ToolError::ExecutionFailed(
                content.to_string(),
            ));
        }
        Ok(manox_harness::tool::AgentToolResult::text(
            content.to_string(),
        ))
    }
}
