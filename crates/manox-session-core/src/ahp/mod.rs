//! Read-only journal → AHP channel-state fold (W2).
//!
//! AHP channel state is a deterministic fold of the durable journal (§C,
//! L3/L10): the same [`Translator`] and the same `ahp` reducers that the host
//! and clients run, driven over one journal from its first entry. These two
//! read-only seams answer "what is this session/thread, in AHP terms, right
//! now" without touching the engine or the live host:
//!
//! - [`chat_state`]: one manox **session** (a `.jsonl` journal) as an
//!   `ahp-chat:/<id>` [`ChatState`] — turns, ordered response parts, tool
//!   call lifecycles, usage.
//! - [`session_state`]: one manox **thread** as an `ahp-session:/<id>`
//!   [`SessionState`] — title, provider, project, granted working
//!   directories, status bits, the chats catalogue and the active-session
//!   pointer as `defaultChat`.
//!
//! The journal read goes through the gateway's existing seams — no bespoke
//! reader: [`crate::journal_query::cold_read`] (persisted-jsonl direct read,
//! the same one `PageHistory` uses) plus
//! [`crate::translate::wire_entry`] (kernel entry → §C.2 wire vocabulary),
//! and thread metadata comes from `manox_agent::thread_store` (the sidebar
//! mirror row) and `manox_agent::thread_registry` (the active-session
//! pointer), the same sources `ListThreads` answers from.
//!
//! # What is deliberately absent
//!
//! A read-only fold can only know what the journal and the store seams
//! carry, and absent fields stay absent rather than being fabricated:
//!
//! - `SessionState.annotations`, `server_tools`, `activeClients`,
//!   `customizations`, `changesets`, `origin`, and the `config` *schema*:
//!   live connection/host facts with no durable journal row. The fold's
//!   `config` **values** (model / effort / approval mode / project /
//!   effective cwd) do arrive, via the translator's `session/configChanged`
//!   actions, on an empty schema the values merge into.
//! - Pin and label: AHP mints no standard bit for them, so the translator
//!   routes them to the declared `x-manox` extension surface; this fold
//!   applies only standard-channel actions and ignores extension channels.
//! - Created/modified timestamps: `SessionState` carries no such fields
//!   (the spec puts them on the root channel's `SessionSummary`); the fold
//!   stamps `ChatState.modifiedAt` with the journal's last entry timestamp,
//!   which keeps `snapshot == fold(replay)` deterministic.
//! - Chat drafts and steering: ephemeral client input state, never
//!   journalled as chat state.
//! - Token *totals* / cost: AHP usage is per-turn (`turn.usage`, folded
//!   from the assistant rows); session-wide aggregation belongs to
//!   `x-manox-metrics`, which is out of scope here.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use ahp::reducers::{apply_action_to_chat, apply_action_to_session};
use ahp_types::actions::StateAction;
use ahp_types::common::JsonObject;
use ahp_types::state::{
    ChatState, ProjectInfo, SessionConfigSchema, SessionConfigState, SessionLifecycle,
    SessionState, SessionStatus,
};
use manox_ahp::channels::{chat, session};
use manox_ahp::translate::Translator;
use manox_journal::JournalWireEvent;

/// The AHP chat state of one manox session, folded from its journal.
///
/// `None` when the session has no persisted journal (or it is unreadable —
/// a corrupt file is a not-found-class answer for this read-only seam,
/// never a silently empty state).
pub async fn chat_state(session_id: &str) -> Option<ChatState> {
    let mut fold = fold_journal(session_id, session_id).await?;
    // The per-session title is sidecar truth (the journal's `title` row is
    // thread-level and rides the session channel), so it is overlaid, not
    // folded. Best effort: an absent sidecar leaves the empty title.
    if let Some(path) = crate::agent_server::persisted_session_file(session_id)
        && let Some(dir) = path.parent()
        && let Ok(meta) = manox_harness::session_meta::load(dir, &path).await
        && let Some(title) = meta.title
    {
        fold.chat.title = title;
    }
    Some(fold.chat)
}

/// The AHP session state of one manox thread, folded from the journals of
/// the thread's sessions plus the sidebar store's metadata row.
///
/// `None` when the store has no row for `thread_id` (unknown thread). A
/// thread whose journals are all unreadable still answers: the catalogue
/// and transcript folds are simply empty.
pub async fn session_state(thread_id: &str) -> Option<SessionState> {
    let store = manox_agent::thread_store::try_global()?;
    let row = store.read(|s| {
        s.summary_by_id(thread_id).cloned().or_else(|| {
            s.archived_summaries()
                .iter()
                .find(|t| t.id == thread_id)
                .cloned()
        })
    })?;
    let running = store.read(|s| s.is_running(thread_id));
    let pending_auth = store.read(|s| s.pending_auth_contains(thread_id));

    // The thread's chats: every session whose journal header is stamped with
    // this thread, plus the registry's active-session pointer (a freshly
    // swapped session may not carry the stamp yet) and, for legacy
    // singleton rows without any thread stamp, the id itself.
    let registry = manox_agent::thread_registry::load().await;
    let active = registry
        .get(thread_id)
        .map(|entry| entry.active_session.clone())
        .unwrap_or_else(|| thread_id.to_string());
    let members = thread_sessions(thread_id, &active);

    let mut state = session::initial(
        row.provider_id.as_deref().unwrap_or_default(),
        seed_working_directories(&active).await,
        Some(empty_config()),
    );
    state.title = row.display_title().to_string();
    if !row.project.is_empty() {
        state.project = Some(project_info(&row.project));
    }
    state.lifecycle = SessionLifecycle::Ready;

    // Fold every member journal; the session-channel actions are the part
    // of each fold that belongs to the thread. Deterministic order (§C
    // L10): members come from a sorted set, actions in journal order.
    let mut chats = Vec::new();
    let mut journal_model: Option<String> = None;
    for id in members {
        let Some(fold) = fold_journal(&id, thread_id).await else {
            continue;
        };
        for action in &fold.session_actions {
            let _ = apply_action_to_session(&mut state, action);
        }
        if journal_model.is_none() {
            journal_model = fold.model_ref;
        }
        chats.push(chat::summary(&fold.chat));
    }
    state.chats = chats;
    state.default_chat = Some(chat::uri(&active));

    if state.provider.is_empty() {
        state.provider = journal_model
            .as_deref()
            .and_then(provider_of_model_ref)
            .unwrap_or_default();
    }
    state.status = session_status_bits(&state, &row, running, pending_auth);
    Some(state)
}

/// One journal's fold: the chat state plus everything the fold routed to
/// the thread's session channel (and the last model reference, which the
/// chat state itself cannot carry).
pub(crate) struct JournalFold {
    pub(crate) chat: ChatState,
    pub(crate) session_actions: Vec<StateAction>,
    /// Every `x-manox*` emission of this journal, in order, with its channel.
    /// The AHP reducers do not fold these, so a late subscriber's baseline is
    /// built by replaying them through [`crate::ext::reducer`].
    pub(crate) extension_actions: Vec<(String, StateAction)>,
    pub(crate) model_ref: Option<String>,
    /// The journal's dense tail (`JournalSnapshotData::cursor`): the highest seq
    /// the fold consumed. A live bridge forwards feed events strictly above it,
    /// which is what keeps a seeded snapshot and the following actions disjoint.
    pub(crate) tail: u64,
}

/// Fold one session's journal through the shared translator + reducers.
///
/// `chat_id` and `thread_id` only steer the emitted actions' channel URIs;
/// the chat fold keeps actions on `ahp-chat:/<chat_id>` and parks the rest
/// for the session fold. Actions on `x-manox-*` extension channels
/// (plan / work / metrics) are dropped here — the fold delivers standard
/// AHP state only.
pub(crate) async fn fold_journal(chat_id: &str, thread_id: &str) -> Option<JournalFold> {
    let snapshot = match crate::journal_query::cold_read(chat_id).await {
        crate::journal_query::ColdRead::Data(snapshot) => snapshot,
        crate::journal_query::ColdRead::NotFound => return None,
        crate::journal_query::ColdRead::Corrupt(error) => {
            tracing::warn!(chat_id, %error, "ahp fold: journal unreadable, answering not-found");
            return None;
        }
    };
    let mut state = chat::initial(chat_id);
    let mut translator = Translator::new();
    let mut session_actions = Vec::new();
    let mut extension_actions: Vec<(String, StateAction)> = Vec::new();
    let mut model_ref: Option<String> = None;
    let mut last_timestamp: Option<String> = None;
    for record in &snapshot.records {
        let Some(entry) = crate::translate::wire_entry(record.seq, &record.entry) else {
            continue;
        };
        // The canonical `provider/model` reference (L8): the wire event
        // carries it as a config value, and the session fold reads the
        // provider out of it when the store row has none.
        if let JournalWireEvent::ModelChange { to, .. } = &entry.event {
            model_ref = Some(to.0.clone());
        }
        for emitted in translator.on_entry(chat_id, thread_id, &entry) {
            if session::id(&emitted.channel).is_some() {
                session_actions.push(emitted.action);
            } else if chat::id(&emitted.channel) == Some(chat_id) {
                let _ = apply_action_to_chat(&mut state, &emitted.action);
            } else {
                // Extension (`x-manox*`) channels carry their own state and no
                // AHP reducer folds them. A subscriber that arrives late needs
                // the result of these emissions, not the stream, so they are
                // kept for the baseline (see `extension_baseline`).
                extension_actions.push((emitted.channel, emitted.action));
            }
        }
        last_timestamp = Some(entry.timestamp);
    }
    // A pure fold must not observe wall-clock time: `chat::initial` stamps
    // "now", the journal's own last entry replaces it. A never-written
    // journal keeps the initial stamp.
    if let Some(stamp) = last_timestamp {
        state.modified_at = stamp;
    }
    Some(JournalFold {
        chat: state,
        session_actions,
        extension_actions,
        model_ref,
        tail: snapshot.cursor,
    })
}

/// The session ids belonging to one thread: journal-header thread stamps,
/// the active pointer, and the legacy singleton's own id — sorted, so the
/// fold order (and therefore the resulting state) is deterministic.
fn thread_sessions(thread_id: &str, active: &str) -> Vec<String> {
    let mut members: BTreeSet<String> = BTreeSet::new();
    members.insert(thread_id.to_string());
    members.insert(active.to_string());
    let Some(dir) = sessions_dir() else {
        return members.into_iter().collect();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return members.into_iter().collect();
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(stem) = session_stem(&path) else {
            continue;
        };
        if members.contains(stem) {
            continue;
        }
        if header_thread_id(&path).is_some_and(|thread| thread == thread_id) {
            members.insert(stem.to_string());
        }
    }
    members.into_iter().collect()
}

/// The `<id>.jsonl` stem of a sessions-dir entry (non-journals yield `None`).
fn session_stem(path: &Path) -> Option<&str> {
    (path.extension()? == "jsonl")
        .then(|| path.file_stem()?.to_str())
        .flatten()
}

/// The thread stamp inside a session file's header line
/// (`{"type":"session", ..., "metadata": {"thread": "..."}}`), `None` when
/// the file is not a readable stamped journal.
fn header_thread_id(path: &Path) -> Option<String> {
    let header = read_header(path)?;
    header
        .metadata
        .as_ref()?
        .get("thread")
        .and_then(|t| t.as_str())
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

/// Line 0 of a journal, decoded through the harness's own header parser.
fn read_header(path: &Path) -> Option<manox_harness::session::jsonl::JsonlSessionMetadata> {
    use std::io::BufRead;
    let file = std::fs::File::open(path).ok()?;
    let mut line = String::new();
    std::io::BufReader::new(file).read_line(&mut line).ok()?;
    let (header, _) = manox_harness::session::jsonl::parse_header_line(line.as_bytes()).ok()?;
    Some(header)
}

/// The directory the durable journals live in — the same resolution
/// [`crate::agent_server::persisted_session_file`] uses, so enumeration and
/// cold read can never disagree.
fn sessions_dir() -> Option<PathBuf> {
    manox_agent::thread_store::try_global()
        .map(|_| manox_agent::thread_store::global_sessions_dir())
        .or_else(|| manox_agent::paths::sessions_dir().ok())
}

/// The seeded granted set: the session's creation cwd plus the sidecar's
/// multi-root grants. Per-directory `cwdChange` grants join via the fold.
async fn seed_working_directories(session_id: &str) -> Option<Vec<String>> {
    let path = crate::agent_server::persisted_session_file(session_id)?;
    let mut dirs = Vec::new();
    if let Some(header) = read_header(&path) {
        dirs.push(file_uri(&header.cwd));
    }
    if let Some(dir) = path.parent()
        && let Ok(meta) = manox_harness::session_meta::load(dir, &path).await
    {
        for granted in meta.working_directories {
            let uri = file_uri(&granted);
            if !dirs.contains(&uri) {
                dirs.push(uri);
            }
        }
    }
    (!dirs.is_empty()).then_some(dirs)
}

/// A values-only config container: the fold has no live `ConfigSchema` to
/// advertise (host-runtime truth), but the reducer merges
/// `session/configChanged` values only into an existing container, so the
/// journal-carried values need this seat to land in.
fn empty_config() -> SessionConfigState {
    SessionConfigState {
        schema: SessionConfigSchema {
            r#type: "object".to_string(),
            properties: HashMap::new(),
            required: None,
        },
        values: JsonObject::new(),
    }
}

/// The provider prefix of a canonical `provider/model` reference (L8).
fn provider_of_model_ref(model_ref: &str) -> Option<String> {
    let (provider, _) = model_ref.split_once('/')?;
    (!provider.is_empty()).then(|| provider.to_string())
}

/// A directory path as the `file://` URI AHP's working-directory and project
/// fields want (the translator's own mint, reproduced here because it is
/// private to the fold that shares the wire shape).
fn file_uri(path: &str) -> String {
    if path.contains("://") {
        return path.to_string();
    }
    format!("file:///{}", path.trim_start_matches('/'))
}

/// A project binding for the session state (name = last path component).
fn project_info(project: &str) -> ProjectInfo {
    ProjectInfo {
        uri: file_uri(project),
        display_name: Path::new(project)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(project)
            .to_string(),
    }
}

/// The session status bitset after the fold.
///
/// The fold may already carry `isArchived` (a journalled `pinnedArchived`
/// row); the live flags come from the store, which owns them (running,
/// pending-auth, unread, errored, archived). Pin has no AHP bit at all —
/// the translator publishes it as an `x-manox` extension action, which this
/// standard-state fold does not consume.
fn session_status_bits(
    folded: &SessionState,
    row: &manox_agent::db::ThreadSummary,
    running: bool,
    pending_auth: bool,
) -> u32 {
    let mut bits = SessionStatus::Idle.bits();
    if row.errored {
        bits |= SessionStatus::Error.bits();
    }
    if running {
        bits |= SessionStatus::InProgress.bits();
    }
    if pending_auth {
        bits |= SessionStatus::InputNeeded.bits();
    }
    if !row.has_unread {
        bits |= SessionStatus::IsRead.bits();
    }
    if row.archived || folded.status & SessionStatus::IsArchived.bits() != 0 {
        bits |= SessionStatus::IsArchived.bits();
    }
    bits
}

/// The thread a session's journal belongs to.
///
/// Precedence mirrors the fold's membership rule: the registry's active-session
/// pointer (a freshly swapped session may not carry the header stamp yet), then
/// the journal header's own thread stamp, then the legacy singleton case where
/// the session *is* the thread.
pub(crate) async fn thread_of_session(session_id: &str) -> Option<String> {
    let registry = manox_agent::thread_registry::load().await;
    for (thread_id, entry) in registry.iter() {
        if entry.active_session == session_id {
            return Some(thread_id.clone());
        }
    }
    if let Some(path) = crate::agent_server::persisted_session_file(session_id)
        && let Some(thread_id) = header_thread_id(&path)
    {
        return Some(thread_id);
    }
    manox_agent::thread_store::try_global()
        .filter(|store| store.read(|state| state.summary_by_id(session_id).is_some()))
        .map(|_| session_id.to_string())
}

mod backend;
mod resources;
pub mod runtime;

/// The AHP session URI of a manox session id.
///
/// A terminal records the session that spawned it, and AHP addresses a session
/// by URI, so the terminal claim needs the one place that spells that mapping.
#[cfg(feature = "terminal")]
pub(crate) fn session_uri_of_terminal(session_id: &str) -> String {
    manox_ahp::channels::session::uri(session_id)
}

#[cfg(test)]
mod tests;
