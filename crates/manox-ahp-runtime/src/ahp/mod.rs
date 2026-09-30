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
//!   `changesets`, `origin`, and the `config` *schema*: live connection/host
//!   facts with no durable journal row. The fold's `config` **values**
//!   (model / effort / approval mode / project / effective cwd) do arrive,
//!   via the translator's `session/configChanged` actions, on an empty
//!   schema the values merge into. `customizations` is the one exception:
//!   the MCP registry is a live *process* fact, so its snapshot overlays the
//!   fold at seed time and the registry's event stream keeps it current —
//!   see [`mcp`].
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
use serde_json::Value;

pub(crate) mod baseline_cache;
pub(crate) mod changeset;
#[cfg(feature = "mcp")]
pub(crate) mod mcp;

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
    if let Some(path) = crate::paths::persisted_session_file(session_id)
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

    // MCP servers are a live process fact, not journal state: the registry
    // snapshot overlays the fold so a fresh seed and the running pump speak
    // from the same source (§C's determinism is journal-scoped; this overlay
    // is the registry's own state at seed time).
    #[cfg(feature = "mcp")]
    {
        let customizations = mcp::customizations();
        if !customizations.is_empty() {
            state.customizations = Some(
                customizations
                    .into_iter()
                    .map(|server| ahp_types::state::Customization::McpServer(Box::new(server)))
                    .collect(),
            );
        }
        let server_tools = mcp::server_tools();
        if !server_tools.is_empty() {
            state.server_tools = Some(server_tools);
        }
    }

    // The uncommitted-changeset catalogue is engine truth like the registry:
    // overlay the entry when the session's directories hold a git repo. The
    // scan is a handful of git invocations and this is the subscribe-seed
    // path, not the list path.
    if let Some(directories) = seed_working_directories(thread_id).await {
        let dirs: Vec<std::path::PathBuf> = directories
            .iter()
            .filter_map(|directory| crate::ahp::backend::file_uri_to_path(directory))
            .map(std::path::PathBuf::from)
            .collect();
        if !dirs.is_empty()
            && let Some(entry) = changeset::Engine::global().catalogue(thread_id, dirs)
        {
            state.changesets = Some(vec![entry]);
        }
    }

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
    /// The last consumed entry's RFC3339 (millis, UTC) stamp — the journal's
    /// recency for baseline replay ordering, where string comparison is
    /// chronological because every stamp shares one format.
    pub(crate) last_timestamp: Option<String>,
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
    if let Some(stamp) = last_timestamp.clone() {
        state.modified_at = stamp;
    }
    Some(JournalFold {
        chat: state,
        session_actions,
        extension_actions,
        model_ref,
        last_timestamp,
        tail: snapshot.cursor,
    })
}

/// The open turn's owning process for one chat journal: the last
/// `turnStart` row's owner stamp, cleared by any later `turnFinish` — the
/// same lifecycle the chat fold gives `active_turn`, read from the raw rows
/// because the AHP fold drops the action meta (an attach-time reader could
/// not reach it there). Answers `None` for an unknown/unreadable journal and
/// for a journal whose last turn closed; callers treat both as "no owner"
/// and never auto-cancel on that account.
pub(crate) async fn open_turn_owner(chat_id: &str) -> Option<manox_journal::TurnOwner> {
    let snapshot = match crate::journal_query::cold_read(chat_id).await {
        crate::journal_query::ColdRead::Data(snapshot) => snapshot,
        crate::journal_query::ColdRead::NotFound => return None,
        crate::journal_query::ColdRead::Corrupt(error) => {
            tracing::warn!(chat_id, %error, "open turn owner: journal unreadable");
            return None;
        }
    };
    let mut owner = None;
    for record in &snapshot.records {
        match &record.entry {
            manox_harness::session::SessionTreeEntry::TurnStart { owner: stamped, .. } => {
                // The single manual field mapping between the harness's
                // local TurnOwner mirror and the journal's (the W4
                // conversion point) — guarded in translate.rs tests by
                // `the_legacy_turn_start_translation_carries_the_owner_pid`.
                owner = stamped.map(|o| manox_journal::TurnOwner { pid: o.pid });
            }
            manox_harness::session::SessionTreeEntry::TurnFinish { .. } => owner = None,
            _ => {}
        }
    }
    owner
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

/// The fresh, thread-scoped fold of one session-scoped `x-manox*` extension
/// channel: every member journal of the owning thread, replayed for exactly
/// the requested channel.
///
/// Two properties the seed cache cannot give a reconnecting client: thread
/// scoping — after a thread continues into a new session, the active
/// session's journal carries rows the URI's own journal lacks — and
/// freshness — the baseline envelope carries the current watermark, so a
/// stale fold would be accepted as current truth. Live emissions key
/// extension channels by thread id (the bridge and [`fold_journal`] both
/// pass the thread id as the translator's session parameter), so rows are
/// replayed only when their channel matches the request verbatim.
///
/// The fold is pure over the member journals' bytes, so it is cached behind
/// a fingerprint of those bytes ([`baseline_cache`]): an unchanged member
/// set with unchanged stamps answers from cache, and any append, rewrite,
/// deletion, or active-session flip misses and refolds. The cache never
/// weakens the freshness property — it only skips a fold whose result is
/// provably identical.
pub(crate) async fn extension_channel_baseline(channel: &str, session_id: &str) -> Value {
    let thread_id = thread_of_session(session_id)
        .await
        .unwrap_or_else(|| session_id.to_string());
    let registry = manox_agent::thread_registry::load().await;
    let active = registry
        .get(&thread_id)
        .map(|entry| entry.active_session.clone())
        .unwrap_or_else(|| session_id.to_string());
    let members = thread_sessions(&thread_id, &active);
    // `sessions_dir()` resolves through the global thread store before
    // falling back to the home — the same resolution the fold reads with, so
    // two state roots never share a cache key.
    let root = sessions_dir()
        .map(|dir| dir.to_string_lossy().into_owned())
        .unwrap_or_default();
    let stamp = baseline_cache::fingerprint(&active, &members);
    if let Some(hit) = baseline_cache::hit(&root, &thread_id, channel, &stamp) {
        baseline_cache::note(true);
        return hit;
    }
    baseline_cache::note(false);
    let value =
        serde_json::to_value(&fold_extension_state(channel, &thread_id, &active, &members).await)
            .unwrap_or(Value::Null);
    baseline_cache::store(&root, &thread_id, channel, stamp, value.clone());
    value
}

/// The fold proper, over an already-enumerated member list: non-active
/// members replay in journal-time order, then the active session's journal.
///
/// Non-active members fold in JOURNAL TIME order, not id order: session ids
/// are uuids whose dictionary order is unrelated to time, and the scalar
/// rows are last-writer-wins — id order let a time-older journal overwrite a
/// newer value (probe-proven). Each journal is folded once and only the
/// requested channel's rows are kept. `None` stamps (empty journals) sort
/// first: nothing to win with. The active session folds last regardless —
/// its journal is the thread's current truth. `thread_id` is the enumeration's
/// thread: the translator keys live emissions by it, so member rows replay
/// only when their channel matches the request verbatim.
async fn fold_extension_state(
    channel: &str,
    thread_id: &str,
    active: &str,
    members: &[String],
) -> manox_ahp::ext::XManoxState {
    let mut older: Vec<(Option<String>, Vec<StateAction>)> = Vec::new();
    for id in members {
        if id == active {
            continue;
        }
        let Some(fold) = fold_journal(id, thread_id).await else {
            continue;
        };
        let rows = fold
            .extension_actions
            .into_iter()
            .filter(|(row_channel, _)| row_channel == channel)
            .map(|(_, action)| action)
            .collect();
        older.push((fold.last_timestamp, rows));
    }
    older.sort_by(|a, b| a.0.cmp(&b.0));

    let mut state = manox_ahp::ext::XManoxState::default();
    let replay = |state: &mut manox_ahp::ext::XManoxState, rows: &[StateAction]| {
        for action in rows {
            let Ok(value) = serde_json::to_value(action) else {
                continue;
            };
            let _ = manox_ahp::ext::reducer::apply(state, &value);
        }
    };
    for (_, rows) in &older {
        replay(&mut state, rows);
    }
    if let Some(fold) = fold_journal(active, thread_id).await {
        let rows = fold
            .extension_actions
            .into_iter()
            .filter(|(row_channel, _)| row_channel == channel)
            .map(|(_, action)| action)
            .collect::<Vec<_>>();
        replay(&mut state, &rows);
    }
    state
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
/// [`crate::paths::persisted_session_file`] uses, so enumeration and
/// cold read can never disagree.
fn sessions_dir() -> Option<PathBuf> {
    manox_agent::thread_store::try_global()
        .map(|_| manox_agent::thread_store::global_sessions_dir())
        .or_else(|| manox_agent::paths::sessions_dir().ok())
}

/// The seeded granted set: the session's creation cwd plus the sidecar's
/// multi-root grants. Per-directory `cwdChange` grants join via the fold.
async fn seed_working_directories(session_id: &str) -> Option<Vec<String>> {
    let path = crate::paths::persisted_session_file(session_id)?;
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
    if let Some(path) = crate::paths::persisted_session_file(session_id)
        && let Some(thread_id) = header_thread_id(&path)
    {
        return Some(thread_id);
    }
    manox_agent::thread_store::try_global()
        .filter(|store| store.read(|state| state.summary_by_id(session_id).is_some()))
        .map(|_| session_id.to_string())
}

pub mod backend;
mod resources;
pub mod runtime;

/// The AHP session URI of a manox session id.
///
/// A terminal records the session that spawned it, and AHP addresses a session
/// by URI, so the terminal claim needs the one place that spells that mapping.
#[cfg(feature = "terminal")]
pub fn session_uri_of_terminal(session_id: &str) -> String {
    manox_ahp::channels::session::uri(session_id)
}

#[cfg(test)]
mod baseline_tests {
    use super::*;
    use manox_harness::session::SessionStorage;

    /// Regression for the dictionary-order fold: member session ids are
    /// uuids, so id order carries no time information, and the scalar rows
    /// are last-writer-wins — folding members by id let a time-older journal
    /// overwrite a newer value. Here the ids sort in the OPPOSITE order of
    /// their journal times, and the newest label must win; the active
    /// session (no label row) must not disturb it.
    #[test]
    fn the_newest_member_journal_wins_scalar_rows_regardless_of_id_order() {
        let _g = crate::test_support::lock_globals();
        crate::test_support::hermetic_home();
        crate::test_support::init_globals();
        manox_agent::runtime::handle().block_on(async {
            let sessions = sessions_dir().expect("sessions dir under the hermetic home");
            std::fs::create_dir_all(&sessions).unwrap();
            // "a_mid" sorts before "z_old" but is written an hour later.
            let members = [
                ("z_old", chrono::Duration::hours(2), "olderTime"),
                ("a_mid", chrono::Duration::hours(1), "newerTime"),
            ];
            for (id, ago, label) in members {
                let path = sessions.join(format!("{id}.jsonl"));
                let _ = std::fs::remove_file(&path);
                let storage = manox_harness::session::jsonl::JsonlSessionStorage::create(
                    &path,
                    manox_harness::session::jsonl::JsonlSessionMetadata {
                        id: id.into(),
                        cwd: "/".into(),
                        created_at: chrono::Utc::now() - ago,
                        parent_session_path: None,
                        metadata: Some(serde_json::json!({"thread": "T-ord"})),
                    },
                )
                .await
                .unwrap();
                storage
                    .append_entry(&manox_harness::session::SessionTreeEntry::Label {
                        id: format!("e-{id}"),
                        parent_id: None,
                        timestamp: chrono::Utc::now() - ago,
                        target_id: format!("e-{id}"),
                        label: Some(label.into()),
                    })
                    .await
                    .unwrap();
                drop(storage);
            }
            // The active session: newest journal, no label row of its own.
            let active_path = sessions.join("m_act.jsonl");
            let _ = std::fs::remove_file(&active_path);
            let active_storage = manox_harness::session::jsonl::JsonlSessionStorage::create(
                &active_path,
                manox_harness::session::jsonl::JsonlSessionMetadata {
                    id: "m_act".into(),
                    cwd: "/".into(),
                    created_at: chrono::Utc::now(),
                    parent_session_path: None,
                    metadata: Some(serde_json::json!({"thread": "T-ord"})),
                },
            )
            .await
            .unwrap();
            active_storage
                .append_entry(&manox_harness::session::SessionTreeEntry::CustomMessage {
                    id: "e-act".into(),
                    parent_id: None,
                    timestamp: chrono::Utc::now(),
                    custom_type: "embedder_seed".into(),
                    content: vec![manox_harness::types::ContentBlock::Text {
                        text: "active".into(),
                        signature: None,
                    }],
                    details: None,
                    display: false,
                })
                .await
                .unwrap();
            drop(active_storage);
            // The thread registry points T-ord at the active session.
            let registry = manox_agent::thread_registry::registry_path();
            std::fs::create_dir_all(registry.parent().unwrap()).unwrap();
            std::fs::write(
                &registry,
                serde_json::json!({"T-ord": {"active_session": "m_act"}}).to_string(),
            )
            .unwrap();

            let baseline = extension_channel_baseline("x-manox-thread:/T-ord", "m_act").await;
            assert_eq!(
                baseline["label"], "newerTime",
                "the time-newer member journal must win, not the id-later one: {baseline}"
            );

            for id in ["z_old", "a_mid", "m_act"] {
                let _ = std::fs::remove_file(sessions.join(format!("{id}.jsonl")));
            }
            let _ = std::fs::remove_file(&registry);
        })
    }

    /// One label journal with a thread-stamped header, at a chosen time.
    async fn write_label_journal(
        sessions: &std::path::Path,
        id: &str,
        thread: &str,
        at: chrono::DateTime<chrono::Utc>,
        label: &str,
    ) {
        let path = sessions.join(format!("{id}.jsonl"));
        let _ = std::fs::remove_file(&path);
        let storage = manox_harness::session::jsonl::JsonlSessionStorage::create(
            &path,
            manox_harness::session::jsonl::JsonlSessionMetadata {
                id: id.into(),
                cwd: "/".into(),
                created_at: at,
                parent_session_path: None,
                metadata: Some(serde_json::json!({ "thread": thread })),
            },
        )
        .await
        .unwrap();
        storage
            .append_entry(&manox_harness::session::SessionTreeEntry::Label {
                id: format!("e-{id}"),
                parent_id: None,
                timestamp: at,
                target_id: format!("e-{id}"),
                label: Some(label.into()),
            })
            .await
            .unwrap();
        drop(storage);
    }

    /// A journal whose lifecycle tail is under test: `entries` are appended
    /// onto a fresh session file with the given turn owner on every start.
    async fn write_turn_journal(
        sessions: &std::path::Path,
        id: &str,
        owner: Option<u32>,
        finish: bool,
    ) {
        let path = sessions.join(format!("{id}.jsonl"));
        let _ = std::fs::remove_file(&path);
        let storage = manox_harness::session::jsonl::JsonlSessionStorage::create(
            &path,
            manox_harness::session::jsonl::JsonlSessionMetadata {
                id: id.into(),
                cwd: "/".into(),
                created_at: chrono::Utc::now(),
                parent_session_path: None,
                metadata: Some(serde_json::json!({ "thread": id })),
            },
        )
        .await
        .unwrap();
        let owner_json = owner.map(|pid| serde_json::json!({ "pid": pid }));
        storage
            .append_entry(
                &serde_json::from_value(serde_json::json!({
                    "type": "turn_start",
                    "id": format!("e-{id}"),
                    "parentId": null,
                    "timestamp": chrono::Utc::now().to_rfc3339(),
                    "owner": owner_json,
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        if finish {
            storage
                .append_entry(&manox_harness::session::SessionTreeEntry::TurnFinish {
                    id: format!("f-{id}"),
                    parent_id: Some(format!("e-{id}")),
                    timestamp: chrono::Utc::now(),
                    cancelled: false,
                    failed: false,
                    stranded_steer_ids: vec![],
                })
                .await
                .unwrap();
        }
        drop(storage);
    }

    fn write_registry(thread: &str, active: &str) -> std::path::PathBuf {
        let registry = manox_agent::thread_registry::registry_path();
        std::fs::create_dir_all(registry.parent().unwrap()).unwrap();
        std::fs::write(
            &registry,
            serde_json::json!({ thread: { "active_session": active } }).to_string(),
        )
        .unwrap();
        registry
    }

    #[test]
    fn the_open_turn_owner_reads_the_journal_tail() {
        let _g = crate::test_support::lock_globals();
        crate::test_support::hermetic_home();
        crate::test_support::init_globals();
        manox_agent::runtime::handle().block_on(async {
            let sessions = sessions_dir().expect("sessions dir under the hermetic home");
            std::fs::create_dir_all(&sessions).unwrap();

            // A stamped turn start at the tail: the owner answers.
            write_turn_journal(&sessions, "open_s", Some(4242), false).await;
            let owner = open_turn_owner("open_s").await.expect("an open turn");
            assert_eq!(owner.pid, 4242);

            // A turn finish after the start closes it: no owner.
            write_turn_journal(&sessions, "closed_s", Some(4242), true).await;
            assert!(
                open_turn_owner("closed_s").await.is_none(),
                "a finished turn has no owner"
            );

            // No journal file at all: no owner (the conservative answer).
            assert!(open_turn_owner("absent_s").await.is_none());
        })
    }

    /// The wrong-shelf half of the baseline fix: the ACTIVE session carries
    /// the thread's current rows, so its journal must be folded (last) even
    /// though the thread-URI baseline used to answer from the URI's own
    /// journal alone.
    #[test]
    fn the_active_sessions_rows_reach_the_thread_baseline() {
        let _g = crate::test_support::lock_globals();
        crate::test_support::hermetic_home();
        crate::test_support::init_globals();
        manox_agent::runtime::handle().block_on(async {
            let sessions = sessions_dir().expect("sessions dir under the hermetic home");
            std::fs::create_dir_all(&sessions).unwrap();
            write_label_journal(
                &sessions,
                "o_shelf",
                "T-shelf",
                chrono::Utc::now() - chrono::Duration::hours(1),
                "shelfOld",
            )
            .await;
            write_label_journal(&sessions, "act_s", "T-shelf", chrono::Utc::now(), "act").await;
            let registry = write_registry("T-shelf", "act_s");

            let baseline = extension_channel_baseline("x-manox-thread:/T-shelf", "act_s").await;
            assert_eq!(
                baseline["label"], "act",
                "the active session's own row must be in — and win — the baseline: {baseline}"
            );

            let _ = std::fs::remove_file(sessions.join("o_shelf.jsonl"));
            let _ = std::fs::remove_file(sessions.join("act_s.jsonl"));
            let _ = std::fs::remove_file(&registry);
        })
    }

    /// The freshness half: there is no seed cache to grow stale — a row that
    /// lands after a baseline was served must show up in the next one.
    #[test]
    fn a_row_appended_after_a_baseline_shows_up_in_the_next_one() {
        let _g = crate::test_support::lock_globals();
        crate::test_support::hermetic_home();
        crate::test_support::init_globals();
        manox_agent::runtime::handle().block_on(async {
            let sessions = sessions_dir().expect("sessions dir under the hermetic home");
            std::fs::create_dir_all(&sessions).unwrap();
            write_label_journal(
                &sessions,
                "fresh_s",
                "fresh_s",
                chrono::Utc::now() - chrono::Duration::seconds(1),
                "v1",
            )
            .await;
            let registry = write_registry("fresh_s", "fresh_s");

            let first = extension_channel_baseline("x-manox-thread:/fresh_s", "fresh_s").await;
            assert_eq!(first["label"], "v1");

            // Append after the first baseline was served.
            let storage = manox_harness::session::jsonl::JsonlSessionStorage::open(
                &sessions.join("fresh_s.jsonl"),
            )
            .await
            .unwrap();
            storage
                .append_entry(&manox_harness::session::SessionTreeEntry::Label {
                    id: "e-v2".into(),
                    parent_id: None,
                    timestamp: chrono::Utc::now(),
                    target_id: "e-v2".into(),
                    label: Some("v2".into()),
                })
                .await
                .unwrap();
            drop(storage);

            let second = extension_channel_baseline("x-manox-thread:/fresh_s", "fresh_s").await;
            assert_eq!(
                second["label"], "v2",
                "the next baseline must see rows written after the previous one: {second}"
            );

            let _ = std::fs::remove_file(sessions.join("fresh_s.jsonl"));
            let _ = std::fs::remove_file(&registry);
        })
    }

    /// The `x-manox/openTurn` query face: the last stamped `turnStart`
    /// answers, any later `turnFinish` clears it, and a session with no
    /// journal at all answers none — the conservative side a reader never
    /// auto-cancels on.
    #[test]
    fn the_open_turn_owner_reads_the_last_stamp_and_clears_on_finish() {
        let _g = crate::test_support::lock_globals();
        crate::test_support::hermetic_home();
        crate::test_support::init_globals();
        manox_agent::runtime::handle().block_on(async {
            let sessions = sessions_dir().expect("sessions dir under the hermetic home");
            std::fs::create_dir_all(&sessions).unwrap();
            let path = sessions.join("open_turn_s.jsonl");
            let _ = std::fs::remove_file(&path);
            let at = chrono::Utc::now();
            let storage = manox_harness::session::jsonl::JsonlSessionStorage::create(
                &path,
                manox_harness::session::jsonl::JsonlSessionMetadata {
                    id: "open_turn_s".into(),
                    cwd: "/".into(),
                    created_at: at,
                    parent_session_path: None,
                    metadata: Some(serde_json::json!({ "thread": "T-open" })),
                },
            )
            .await
            .unwrap();
            storage
                .append_entry(&manox_harness::session::SessionTreeEntry::TurnStart {
                    id: "e-ts".into(),
                    parent_id: None,
                    timestamp: at,
                    owner: Some(manox_harness::session::TurnOwner { pid: 4242 }),
                })
                .await
                .unwrap();
            drop(storage);

            let owner = open_turn_owner("open_turn_s").await;
            assert_eq!(
                owner.map(|owner| owner.pid),
                Some(4242),
                "the last turnStart's stamp answers the query"
            );

            let storage = manox_harness::session::jsonl::JsonlSessionStorage::open(&path)
                .await
                .unwrap();
            storage
                .append_entry(&manox_harness::session::SessionTreeEntry::TurnFinish {
                    id: "e-tf".into(),
                    parent_id: Some("e-ts".into()),
                    timestamp: chrono::Utc::now(),
                    cancelled: false,
                    failed: false,
                    stranded_steer_ids: Vec::new(),
                })
                .await
                .unwrap();
            drop(storage);

            assert_eq!(
                open_turn_owner("open_turn_s").await,
                None,
                "a later turnFinish clears the owner"
            );
            assert_eq!(
                open_turn_owner("no_such_session").await,
                None,
                "a session with no journal answers no owner"
            );

            let _ = std::fs::remove_file(&path);
        })
    }

    /// The cache actually answers: an unchanged journal set serves the second
    /// request from the cache (a hit, no fold), and any journal move — an
    /// append here — forces the next request to fold again and see the new
    /// row. Without the hit/fold probe this would also pass with the cache
    /// deleted; with it, the freshness test above and this one pin both
    /// halves: the cache must answer AND it must never answer stale.
    #[test]
    fn the_baseline_cache_answers_until_a_journal_moves() {
        let _g = crate::test_support::lock_globals();
        crate::test_support::hermetic_home();
        crate::test_support::init_globals();
        manox_agent::runtime::handle().block_on(async {
            let sessions = sessions_dir().expect("sessions dir under the hermetic home");
            std::fs::create_dir_all(&sessions).unwrap();
            write_label_journal(
                &sessions,
                "cache_s",
                "cache_s",
                chrono::Utc::now() - chrono::Duration::seconds(1),
                "v1",
            )
            .await;
            let registry = write_registry("cache_s", "cache_s");

            let (hits_before, folds_before) = baseline_cache::probe();
            let first = extension_channel_baseline("x-manox-thread:/cache_s", "cache_s").await;
            let second = extension_channel_baseline("x-manox-thread:/cache_s", "cache_s").await;
            assert_eq!(first, second, "unchanged journals fold to the same state");
            let (hits, folds) = baseline_cache::probe();
            assert_eq!(folds, folds_before + 1, "the fold ran exactly once");
            assert_eq!(hits, hits_before + 1, "the second request hit the cache");

            // An append moves the stamp: the cache must miss and refold.
            let storage = manox_harness::session::jsonl::JsonlSessionStorage::open(
                &sessions.join("cache_s.jsonl"),
            )
            .await
            .unwrap();
            storage
                .append_entry(&manox_harness::session::SessionTreeEntry::Label {
                    id: "e-v2".into(),
                    parent_id: None,
                    timestamp: chrono::Utc::now(),
                    target_id: "e-v2".into(),
                    label: Some("v2".into()),
                })
                .await
                .unwrap();
            drop(storage);

            let third = extension_channel_baseline("x-manox-thread:/cache_s", "cache_s").await;
            assert_eq!(
                third["label"], "v2",
                "the moved journal must refold: {third}"
            );
            let (_, folds_after) = baseline_cache::probe();
            assert_eq!(folds_after, folds + 1, "the append forced a refold");

            let _ = std::fs::remove_file(sessions.join("cache_s.jsonl"));
            let _ = std::fs::remove_file(&registry);
        })
    }

    /// Which member folds last is part of the fold's input, not just the
    /// journals' bytes: flipping the active pointer must move the answer even
    /// when no journal changed — here the newly-active journal carries the
    /// winning row only after the flip.
    #[test]
    fn an_active_flip_moves_the_baseline_even_without_journal_changes() {
        let _g = crate::test_support::lock_globals();
        crate::test_support::hermetic_home();
        crate::test_support::init_globals();
        manox_agent::runtime::handle().block_on(async {
            let sessions = sessions_dir().expect("sessions dir under the hermetic home");
            std::fs::create_dir_all(&sessions).unwrap();
            write_label_journal(
                &sessions,
                "flip_a",
                "T-flip",
                chrono::Utc::now() - chrono::Duration::seconds(2),
                "fromA",
            )
            .await;
            write_label_journal(&sessions, "flip_b", "T-flip", chrono::Utc::now(), "fromB").await;
            let registry = write_registry("T-flip", "flip_a");

            let before = extension_channel_baseline("x-manox-thread:/T-flip", "flip_a").await;
            assert_eq!(before["label"], "fromA", "the active journal folds last: {before}");

            write_registry("T-flip", "flip_b");
            let after = extension_channel_baseline("x-manox-thread:/T-flip", "flip_b").await;
            assert_eq!(
                after["label"], "fromB",
                "the active flip must invalidate the cached fold even with unchanged journals: {after}"
            );

            let _ = std::fs::remove_file(sessions.join("flip_a.jsonl"));
            let _ = std::fs::remove_file(sessions.join("flip_b.jsonl"));
            let _ = std::fs::remove_file(&registry);
        })
    }
}
