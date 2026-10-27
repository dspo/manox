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
//! reader: [`crate::journal_query::cold_read`] (persisted-jsonl direct read)
//! plus [`crate::ahp::projection`] (kernel record → AHP action, no
//! intermediate vocabulary), and thread metadata comes from
//! `manox_agent::thread_store` (the sidebar mirror row) and
//! `manox_agent::thread_registry` (the active-session pointer), the same
//! sources `ListThreads` answers from.
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

use crate::ahp::projection::Translator;
use crate::ahp::projection::config_keys;
use ahp::reducers::{apply_action_to_chat, apply_action_to_session};
use ahp_types::actions::StateAction;
use ahp_types::common::JsonObject;
use ahp_types::state::{
    ChatState, ProjectInfo, SessionConfigPropertySchema, SessionConfigSchema, SessionConfigState,
    SessionLifecycle, SessionState, SessionStatus,
};
use manox_ahp::channels::{chat, session};
use manox_harness::sandbox::{AccessMode, NetworkAccess};
use manox_harness::session::SessionTreeEntry;
use serde_json::Value;

pub(crate) mod baseline_cache;
pub(crate) mod changeset;
pub(crate) mod create_intent;
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
/// `None` when the thread is genuinely unknown: no store row, no journal
/// (live or durable) and no create intent. A session this process just
/// created seeds before its store row exists, from the intent the create
/// carried — see [`session_state_with_live`].
pub async fn session_state(thread_id: &str) -> Option<SessionState> {
    session_state_with_live(thread_id, None).await
}

/// [`session_state`] with a live engine's journal snapshot for one member.
///
/// The live snapshot is the session's journal while an engine drives it — a
/// deferred session's create rows live only there, never on disk before the
/// first turn. The caller subscribes to the feed before taking the snapshot,
/// so nothing appended between the two escapes the seed or the following
/// bridge.
///
/// # Authority
///
/// A session's seed stacks three sources, weakest first:
///
/// 1. the **intent overlay** — the durable object row (the desktop writes its
///    draft project and picks before `createSession`) and, when that row is
///    absent, the create intent this process accepted; plus the thread
///    store's pending working-directory grants;
/// 2. the **journal fold** — live memory or durable disk, in member order.
///
/// The overlay is what a deferred session has: before materialization there is
/// no journal to fold, so it is the only honest answer for that window. It is
/// applied FIRST and the fold runs after it, so a journal row always wins
/// (last writer = journal) the moment it exists. This is not a second
/// authority: it seeds only what no row has answered yet.
pub(crate) async fn session_state_with_live(
    thread_id: &str,
    live: Option<(&str, &manox_agent::engine::JournalSnapshotData)>,
) -> Option<SessionState> {
    let store = manox_agent::thread_store::try_global()?;
    let row = store.read(|s| {
        s.summary_by_id(thread_id).cloned().or_else(|| {
            s.archived_summaries()
                .iter()
                .find(|t| t.id == thread_id)
                .cloned()
        })
    });
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

    // The deferred intent, keyed by the session the create minted. A legacy
    // thread whose active pointer names another session keeps the thread's
    // own row in reach.
    let overlay = intent_overlay(&active).or_else(|| {
        (active != thread_id)
            .then(|| intent_overlay(thread_id))
            .flatten()
    });

    // Fold every member journal; the session-channel actions are the part
    // of each fold that belongs to the thread. Deterministic order (§C
    // L10): members come from a sorted set, actions in journal order.
    let mut folds = Vec::new();
    for id in members {
        let fold = match live {
            Some((live_id, snapshot)) if live_id == id => {
                Some(fold_journal_snapshot(&id, thread_id, snapshot).await)
            }
            _ => fold_journal(&id, thread_id).await,
        };
        if let Some(fold) = fold {
            folds.push(fold);
        }
    }

    // A create seeds the session it just minted, and that session's store row
    // only appears once a list refresh scans its file. The journal fold (or
    // the deferred intent behind it) is what the seed answers with meanwhile;
    // only a thread with neither is unknown.
    let row = row.or_else(|| {
        (overlay.is_some() || !folds.is_empty()).then(|| manox_agent::db::ThreadSummary {
            id: thread_id.to_string(),
            ..Default::default()
        })
    })?;

    let mut state = session::initial(
        row.provider_id.as_deref().unwrap_or_default(),
        seed_working_directories(&active).await,
        Some(empty_config()),
    );
    state.title = row.display_title().to_string();
    if !row.project.is_empty() {
        state.project = Some(project_info(&row.project));
    }
    // Intent first, journal second: the fold below overwrites every key a
    // row carries, which is what keeps the journal the last writer.
    if let Some(overlay) = &overlay {
        apply_intent_overlay(&mut state, overlay, &active);
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

    let mut chats = Vec::new();
    let mut journal_model: Option<String> = None;
    for fold in folds {
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
            .or_else(|| {
                overlay
                    .as_ref()
                    .and_then(|overlay| overlay.model.as_deref())
                    .and_then(provider_of_model_ref)
            })
            .unwrap_or_default();
    }
    state.status = row_status_bits(Some(&state), &row, running, pending_auth);
    Some(state)
}

/// The pre-materialization intent of one session: the durable object row
/// (draft project plus the three config picks) and, where it is silent, the
/// create intent this process accepted. The thread store's pending grant set
/// joins here too — a deferred create keeps its working directories in the
/// store until the sidecar can take them.
///
/// `None` when no source has anything to say; that is the "no intent, unknown
/// thread" case the caller answers `None` for.
#[derive(Debug, Default)]
struct IntentOverlay {
    project: Option<String>,
    model: Option<String>,
    access_mode: Option<String>,
    reasoning_effort: Option<String>,
    working_directories: Vec<String>,
}

/// Build the intent overlay for `session_id` (see [`IntentOverlay`]).
fn intent_overlay(session_id: &str) -> Option<IntentOverlay> {
    let object = manox_agent::object_store::ObjectStore::try_global();
    let row = object.and_then(|store| store.conversation(session_id));
    let intent = create_intent::get(session_id);

    let project = object
        .and_then(|store| store.project_of(session_id))
        .or_else(|| intent.as_ref().and_then(|intent| intent.project.clone()));
    let model = row
        .as_ref()
        .and_then(|row| match (&row.model_provider, &row.model_id) {
            (Some(provider), Some(model)) => Some(format!("{provider}/{model}")),
            (None, Some(model)) => Some(model.clone()),
            _ => None,
        })
        .or_else(|| intent.as_ref().and_then(|intent| intent.model.clone()));
    // The row's access-mode column is the wire vocabulary; normalizing
    // through the enum drops a hand-edited value instead of seeding a value
    // no client can select.
    let access_mode = row
        .as_ref()
        .and_then(|row| row.access_mode.as_deref())
        .and_then(AccessMode::from_wire)
        .map(|mode| mode.wire().to_string())
        .or_else(|| {
            intent
                .as_ref()
                .and_then(|intent| intent.access_mode.clone())
        });
    let reasoning_effort = row
        .as_ref()
        .and_then(|row| row.reasoning_effort.clone())
        .or_else(|| {
            intent
                .as_ref()
                .and_then(|intent| intent.reasoning_effort.clone())
        });
    let working_directories = manox_agent::thread_store::try_global()
        .and_then(|store| store.read(|state| state.working_directories(session_id)))
        .or_else(|| {
            intent
                .as_ref()
                .map(|intent| intent.working_directories.clone())
        })
        .unwrap_or_default();

    let empty = project.is_none()
        && model.is_none()
        && access_mode.is_none()
        && reasoning_effort.is_none()
        && working_directories.is_empty();
    (!empty).then_some(IntentOverlay {
        project,
        model,
        access_mode,
        reasoning_effort,
        working_directories,
    })
}

/// Seat the intent overlay on a fresh session state.
///
/// The effective cwd mirrors the engine's own create rule: a project session
/// runs in its project, a project-less fresh session in its per-id unfiled
/// workspace. `workingDirectory` and the granted set carry that plus every
/// directory the create named.
fn apply_intent_overlay(state: &mut SessionState, overlay: &IntentOverlay, session_id: &str) {
    let cwd = overlay.project.clone().unwrap_or_else(|| {
        manox_agent::paths::unfiled_workspace(session_id)
            .to_string_lossy()
            .into_owned()
    });
    let mut directories = state.working_directories.take().unwrap_or_default();
    let cwd_uri = file_uri(&cwd);
    if !directories.contains(&cwd_uri) {
        directories.insert(0, cwd_uri);
    }
    for directory in &overlay.working_directories {
        let uri = file_uri(directory);
        if !directories.contains(&uri) {
            directories.push(uri);
        }
    }
    state.working_directories = (!directories.is_empty()).then_some(directories);
    // The sidebar row's project (when a scan produced one) is the store's
    // answer and stays ahead of intention.
    if state.project.is_none()
        && let Some(project) = &overlay.project
    {
        state.project = Some(project_info(project));
    }
    let config = state.config.get_or_insert_with(empty_config);
    if let Some(model) = &overlay.model {
        config
            .values
            .insert(config_keys::MODEL.to_string(), Value::String(model.clone()));
    }
    if let Some(mode) = &overlay.access_mode {
        config.values.insert(
            config_keys::ACCESS_MODE.to_string(),
            Value::String(mode.clone()),
        );
    }
    if let Some(effort) = &overlay.reasoning_effort {
        config.values.insert(
            config_keys::REASONING_EFFORT.to_string(),
            Value::String(effort.clone()),
        );
    }
    if let Some(project) = &overlay.project {
        config.values.insert(
            config_keys::PROJECT.to_string(),
            Value::String(project.clone()),
        );
    }
    config.values.insert(
        config_keys::WORKING_DIRECTORY.to_string(),
        Value::String(cwd),
    );
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
/// for the session fold. Actions on `x-manox*` extension channels
/// (plan / work / metrics) are dropped here — the fold delivers standard
/// AHP state only.
///
/// This is the durable read. A caller holding a live snapshot (the engine's
/// in-memory journal, which a deferred session has before any file exists)
/// folds that instead through [`fold_journal_snapshot`].
pub(crate) async fn fold_journal(chat_id: &str, thread_id: &str) -> Option<JournalFold> {
    let snapshot = match crate::journal_query::cold_read(chat_id).await {
        crate::journal_query::ColdRead::Data(snapshot) => snapshot,
        crate::journal_query::ColdRead::NotFound => return None,
        crate::journal_query::ColdRead::Corrupt(error) => {
            tracing::warn!(chat_id, %error, "ahp fold: journal unreadable, answering not-found");
            return None;
        }
    };
    Some(fold_journal_snapshot(chat_id, thread_id, &snapshot).await)
}

/// The fold proper, over one already-read snapshot (live memory or durable
/// disk). Split out so a seed that subscribed to the live feed folds the very
/// records that feed will continue from, without a second read.
pub(crate) async fn fold_journal_snapshot(
    chat_id: &str,
    thread_id: &str,
    snapshot: &manox_agent::engine::JournalSnapshotData,
) -> JournalFold {
    let mut state = chat::initial(chat_id);
    // A child chat declares its lineage on the state itself — the spec's
    // reverse edge, kept consistent with the parent tool call's
    // `ToolResultSubagentContent` (MUST). The header metadata the spawn
    // wrote carries the parent and the dispatching tool call id; a worker
    // chat is read-only to the user.
    if let Some(meta) = crate::journal_query::header_metadata(chat_id).await
        && let Some(subagent) = meta.get("subagent")
        && let Some(parent) = subagent.get("parent").and_then(|v| v.as_str())
    {
        let tool_call_id = subagent
            .get("toolCallId")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        state.origin = Some(ahp_types::state::ChatOrigin::Tool {
            chat: format!("ahp-chat:/{parent}"),
            tool_call_id,
        });
        state.interactivity = Some(ahp_types::state::ChatInteractivity::ReadOnly);
    }
    let mut translator = Translator::new();
    let mut session_actions = Vec::new();
    let mut extension_actions: Vec<(String, StateAction)> = Vec::new();
    let mut model_ref: Option<String> = None;
    let mut last_timestamp: Option<String> = None;
    for record in &snapshot.records {
        // The canonical `provider/model` reference (L8): the kernel row
        // carries provider and model id separately, and the session fold
        // reads the provider out of the composed value when the store row
        // has none.
        if let SessionTreeEntry::ModelChange {
            provider, model_id, ..
        } = &record.entry
        {
            model_ref = Some(format!("{provider}/{model_id}"));
        }
        for emitted in translator.on_entry(chat_id, thread_id, record.seq, &record.entry) {
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
        last_timestamp = Some(
            record
                .entry
                .timestamp()
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        );
    }
    // A pure fold must not observe wall-clock time: `chat::initial` stamps
    // "now", the journal's own last entry replaces it. A never-written
    // journal keeps the initial stamp.
    if let Some(stamp) = last_timestamp.clone() {
        state.modified_at = stamp;
    }
    JournalFold {
        chat: state,
        session_actions,
        extension_actions,
        model_ref,
        last_timestamp,
        tail: snapshot.cursor,
    }
}

/// The open turn's owning process for one chat journal: the last
/// `turnStart` row's owner stamp, cleared by any later `turnFinish` — the
/// same lifecycle the chat fold gives `active_turn`, read from the raw rows
/// because the AHP fold drops the action meta (an attach-time reader could
/// not reach it there). Answers `None` for an unknown/unreadable journal and
/// for a journal whose last turn closed; callers treat both as "no owner"
/// and never auto-cancel on that account.
pub(crate) async fn open_turn_owner(chat_id: &str) -> Option<manox_harness::session::TurnOwner> {
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
                owner = *stamped;
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
    // The core layout is two-deep (buckets over per-session directories,
    // `log.v1.jsonl` inside); a session's directory name is its id. The
    // walk stays bounded to that shape.
    fn collect(dir: &Path, depth: u8, thread_id: &str, members: &mut BTreeSet<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            if meta.is_dir() {
                if depth > 0 {
                    collect(&path, depth - 1, thread_id, members);
                }
                continue;
            }
            let Some(stem) = session_stem_of(&path) else {
                continue;
            };
            if members.contains(stem.as_str()) {
                continue;
            }
            if header_thread_id(&path).is_some_and(|thread| thread == thread_id) {
                members.insert(stem.to_string());
            }
        }
    }
    collect(&dir, 2, thread_id, &mut members);
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

fn session_stem_of(path: &Path) -> Option<String> {
    // The core layout names the journal by the shared constant; a session's id is
    // its PARENT directory's name. A legacy flat `<id>.jsonl` keeps the
    // file stem.
    if path.file_name().and_then(|n| n.to_str())
        == Some(manox_harness::session::identity::JOURNAL_FILE)
    {
        return path.parent()?.file_name()?.to_str().map(str::to_string);
    }
    (path.extension()? == "jsonl")
        .then(|| path.file_stem()?.to_str().map(str::to_string))
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
    // The v1.0.0 core root ITSELF, never a bucket: production sessions live
    // under `<core>/<project-slug>/<id>/log.v1.jsonl`, and a test store's
    // fixtures may sit one level shallower — the bounded walk below reads
    // both shapes. Naming a bucket here (the fixtures' own `t`, say) would
    // make the enumeration resolve a directory production never writes, so
    // no thread's members would ever be found.
    manox_agent::thread_store::try_global()
        .map(|_| manox_agent::thread_store::global_sessions_dir())
        .or_else(|| manox_agent::paths::core_root().ok())
}

/// The seeded granted set: the session's creation cwd plus the sidecar's
/// multi-root grants. Per-directory `cwdChange` grants join via the fold.
///
/// A deferred session has neither a header nor a sidecar — its create-time
/// grants live only in the thread store until the journal materializes — so
/// the store's pending set is a third leg. The disk legs stay first: once the
/// sidecar exists it is the durable truth, and the store's pending set has
/// already flushed into it.
async fn seed_working_directories(session_id: &str) -> Option<Vec<String>> {
    let mut dirs = Vec::new();
    if let Some(path) = crate::paths::persisted_session_file(session_id) {
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
    }
    if let Some(granted) = manox_agent::thread_store::try_global()
        .and_then(|store| store.read(|state| state.working_directories(session_id)))
    {
        for dir in granted {
            let uri = file_uri(&dir);
            if !dirs.contains(&uri) {
                dirs.push(uri);
            }
        }
    }
    (!dirs.is_empty()).then_some(dirs)
}

/// A config container with this host's declared schema and no values yet:
/// the reducer merges `session/configChanged` values only into an existing
/// container, so the journal-carried values need this seat to land in — and
/// the schema is the `sessionMutable` contract a compliant client reads
/// before offering a mid-session change.
fn empty_config() -> SessionConfigState {
    SessionConfigState {
        schema: session_config_schema(),
        values: JsonObject::new(),
    }
}

/// The session config schema this host declares on every session: the three
/// selection keys the runtime owns. The property set MUST stay identical to
/// [`config_keys::MUTABLE`] — the dispatch gate accepts exactly the
/// session-mutable keys, so the two sources drifting would advertise a
/// contract the host refuses to honor (pinned by test).
fn session_config_schema() -> SessionConfigSchema {
    fn prop(
        title: &str,
        description: Option<&str>,
        values: Option<&[&str]>,
        default: Option<&str>,
    ) -> SessionConfigPropertySchema {
        SessionConfigPropertySchema {
            r#type: "string".to_string(),
            title: title.to_string(),
            description: description.map(str::to_string),
            default: default.map(|value| Value::String(value.to_string())),
            // The enum values are the protocol's wire vocabulary; labels
            // stay unset — display naming is the client's (localized)
            // business, and a wire-string label would only look like one.
            r#enum: values.map(|values| {
                values
                    .iter()
                    .map(|value| Value::String(value.to_string()))
                    .collect()
            }),
            enum_labels: None,
            enum_descriptions: None,
            read_only: None,
            items: None,
            min_items: None,
            max_items: None,
            properties: None,
            required: None,
            additional_properties: None,
            enum_dynamic: None,
            session_mutable: Some(true),
        }
    }
    let mut properties = HashMap::new();
    properties.insert(
        config_keys::MODEL.to_string(),
        prop(
            "Model",
            Some("Canonical `provider/model` reference"),
            None,
            None,
        ),
    );
    properties.insert(
        config_keys::ACCESS_MODE.to_string(),
        // The wire vocabulary is `AccessMode`'s kebab form; the default
        // is the host's own (`AccessMode::default()`). Parity is pinned
        // by test, not trusted.
        prop(
            "Access mode",
            None,
            Some(&[AccessMode::Sandbox.wire(), AccessMode::FullAccess.wire()]),
            Some(AccessMode::default().wire()),
        ),
    );
    properties.insert(
        config_keys::NETWORK_ACCESS.to_string(),
        // The network switch for sandboxed bash; the default is the
        // vocabulary's own closed default. Parity is pinned by test.
        prop(
            "Network access",
            Some("Network reachability for sandboxed commands"),
            Some(&[NetworkAccess::Off.wire(), NetworkAccess::On.wire()]),
            Some(NetworkAccess::default().wire()),
        ),
    );
    properties.insert(
        config_keys::REASONING_EFFORT.to_string(),
        // The enum is the WRITABLE subset (`set_reasoning_effort` accepts
        // high|max); the seated value may carry creation-time wire levels
        // outside it, so a client must treat out-of-enum values as
        // displayable-but-not-selectable, not as invalid.
        prop(
            "Reasoning effort",
            Some("Writable subset; the current value may carry a host-default level outside this set"),
            Some(&["high", "max"]),
            None,
        ),
    );
    SessionConfigSchema {
        r#type: "object".to_string(),
        properties,
        required: None,
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
/// pending-auth, the derived read bit, errored, archived). Pin has no AHP
/// bit at all — the translator publishes it as an `x-manox` extension
/// action, which this standard-state fold does not consume. `None` fold =
/// a parked row (this host never folded it): the store carries everything
/// the bit table can express, which is exactly why the parked answer and
/// the live answer must come from ONE table — two tables drift.
pub(crate) fn row_status_bits(
    folded: Option<&SessionState>,
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
    if row.archived
        || folded.is_some_and(|folded| folded.status & SessionStatus::IsArchived.bits() != 0)
    {
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
    let journal = crate::paths::persisted_session_file(session_id);
    if let Some(path) = journal.as_deref()
        && let Some(thread_id) = header_thread_id(path)
    {
        return Some(thread_id);
    }
    // A journal with no thread stamp belongs to itself. v1.0.0 mints one
    // session per conversation (thread id == session id), and a sub-agent's
    // child journal is stamped with its lineage, never with a thread — so
    // the store-row leg below, which needs a list scan to have run, is the
    // only thing standing between a client and a session whose journal is
    // right there. The sub-agent drill-down subscribes straight to the
    // child's chat (`ahp-chat:/<run-id>`) and a reconnecting client can
    // subscribe before any list refresh: both answered `not found` for a
    // chat whose own journal was on disk.
    if journal.is_some() {
        return Some(session_id.to_string());
    }
    manox_agent::thread_store::try_global()
        .filter(|store| store.read(|state| state.summary_by_id(session_id).is_some()))
        .map(|_| session_id.to_string())
}

pub mod backend;
pub mod projection;
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

    fn journal(sessions: &std::path::Path, id: &str) -> std::path::PathBuf {
        sessions
            .join(id)
            .join(manox_harness::session::identity::JOURNAL_FILE)
    }
    use super::*;
    use manox_harness::session::SessionStorage;

    /// The member enumeration reads the core root, not one bucket.
    ///
    /// Production journals live under `<core>/<project-slug>/<id>/
    /// log.v1.jsonl`; a scan pointed at a single bucket (the test fixtures'
    /// own `t`, say) resolves a directory production never writes, so every
    /// thread's members silently collapse to the active pointer — the
    /// multi-session catalog and the member fold lose every other journal.
    /// The fixture here is production-shaped on purpose: it sits under a
    /// real project bucket, which only a root-wide walk finds.
    #[test]
    fn thread_members_are_found_under_a_production_bucket() {
        let _g = crate::test_support::lock_globals();
        crate::test_support::hermetic_home();
        crate::test_support::init_globals();
        manox_agent::runtime::handle().block_on(async {
            // The fixture's location comes from the production root (the
            // path authority `core_root` names), never from the enumeration
            // under test — otherwise a scan pointed at the wrong bucket
            // would move the fixture along with it and the guard would
            // prove nothing.
            let root = manox_agent::paths::core_root().expect("core root");
            let bucket = root.join("users-x-work-manox");
            let path = journal(&bucket, "m-bucketed");
            let _ = std::fs::remove_file(&path);
            let storage = manox_harness::session::jsonl::JsonlSessionStorage::create(
                &path,
                manox_harness::session::jsonl::JsonlSessionMetadata {
                    id: "m-bucketed".into(),
                    cwd: "/Users/x/work/manox".into(),
                    created_at: chrono::Utc::now(),
                    parent_session_path: None,
                    metadata: Some(serde_json::json!({"thread": "T-bucketed"})),
                },
            )
            .await
            .unwrap();
            drop(storage);

            // The active pointer is a DIFFERENT session: the member can only
            // arrive through the disk walk, never through the two ids the
            // enumeration seeds by construction.
            let members = thread_sessions("T-bucketed", "m-active");
            assert!(
                members.contains(&"m-bucketed".to_string()),
                "a member under a production bucket is enumerated: {members:?}"
            );
            let _ = std::fs::remove_file(&path);
        })
    }

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
                let path = journal(&sessions, id);
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
            let active_path = journal(&sessions, "m_act");
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
                let _ = std::fs::remove_file(journal(&sessions, id));
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
        let path = journal(sessions, id);
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
        let path = journal(sessions, id);
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

            let _ = std::fs::remove_file(journal(&sessions, "o_shelf"));
            let _ = std::fs::remove_file(journal(&sessions, "act_s"));
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
            let storage = manox_harness::session::jsonl::JsonlSessionStorage::open(&journal(
                &sessions, "fresh_s",
            ))
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

            let _ = std::fs::remove_file(journal(&sessions, "fresh_s"));
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
            let path = journal(&sessions, "open_turn_s");
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
            let storage = manox_harness::session::jsonl::JsonlSessionStorage::open(&journal(
                &sessions, "cache_s",
            ))
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

            let _ = std::fs::remove_file(journal(&sessions, "cache_s"));
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

            let _ = std::fs::remove_file(journal(&sessions, "flip_a"));
            let _ = std::fs::remove_file(journal(&sessions, "flip_b"));
            let _ = std::fs::remove_file(&registry);
        })
    }
}

#[cfg(test)]
mod config_schema_tests {
    use super::*;

    /// The declared schema and the dispatch gate must name the same keys:
    /// `config_keys::MUTABLE` is what `apply_session_config` accepts, the
    /// schema is what a compliant client reads, and a drift between them
    /// would advertise a contract the host refuses to honor.
    #[test]
    fn the_schema_names_exactly_the_session_mutable_keys() {
        let schema = session_config_schema();
        let mut declared: Vec<&str> = schema.properties.keys().map(String::as_str).collect();
        declared.sort_unstable();
        let mut gate: Vec<&str> = config_keys::MUTABLE.to_vec();
        gate.sort_unstable();
        assert_eq!(declared, gate);
        for (key, prop) in &schema.properties {
            assert_eq!(
                prop.session_mutable,
                Some(true),
                "config key {key} must declare sessionMutable"
            );
        }
    }

    /// The access-mode vocabulary is `AccessMode`'s own wire set with
    /// the host's default — the enum is not a hand-maintained look-alike.
    #[test]
    fn the_access_mode_enum_is_the_access_mode_wire_vocabulary() {
        let schema = session_config_schema();
        let prop = schema
            .properties
            .get(config_keys::ACCESS_MODE)
            .expect("accessMode declared");
        let values: Vec<&str> = prop
            .r#enum
            .as_ref()
            .expect("accessMode enumerates its values")
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert_eq!(
            values,
            vec![AccessMode::Sandbox.wire(), AccessMode::FullAccess.wire()]
        );
        assert_eq!(
            prop.default.as_ref().and_then(Value::as_str),
            Some(AccessMode::default().wire()),
            "the declared default is the host's own default mode"
        );
    }

    /// The network-access vocabulary is `NetworkAccess`'s own wire set with
    /// the closed default — same pin as the access-mode enum above.
    #[test]
    fn the_network_access_enum_is_the_network_wire_vocabulary() {
        let schema = session_config_schema();
        let prop = schema
            .properties
            .get(config_keys::NETWORK_ACCESS)
            .expect("networkAccess declared");
        let values: Vec<&str> = prop
            .r#enum
            .as_ref()
            .expect("networkAccess enumerates its values")
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert_eq!(
            values,
            vec![NetworkAccess::Off.wire(), NetworkAccess::On.wire()]
        );
        assert_eq!(
            prop.default.as_ref().and_then(Value::as_str),
            Some(NetworkAccess::default().wire()),
            "the declared default is the closed default"
        );
    }
}

/// The one bit table: the parked answer (no fold — the store row is all the
/// host knows) and the live answer come from the same arithmetic, so the two
/// faces of one session can never disagree about read, error, or archive.
#[cfg(test)]
mod row_status_bits_tests {
    use super::*;
    use ahp_types::state::SessionStatus;

    fn row(errored: bool, archived: bool, unread: bool) -> manox_agent::db::ThreadSummary {
        manox_agent::db::ThreadSummary {
            errored,
            archived,
            has_unread: unread,
            ..Default::default()
        }
    }

    #[test]
    fn a_parked_row_answers_from_the_store_row_alone() {
        assert_eq!(
            row_status_bits(None, &row(false, false, false), false, false),
            SessionStatus::Idle.bits() | SessionStatus::IsRead.bits(),
            "a plain parked row is idle and read"
        );
        assert_eq!(
            row_status_bits(None, &row(false, false, true), false, false),
            SessionStatus::Idle.bits(),
            "an unread parked row keeps IsRead down — it counts as attention"
        );
        assert_eq!(
            row_status_bits(None, &row(true, false, false), false, false),
            SessionStatus::Idle.bits() | SessionStatus::Error.bits() | SessionStatus::IsRead.bits(),
            "the row's error state shows through the parked answer"
        );
        assert_eq!(
            row_status_bits(None, &row(false, true, false), false, false),
            SessionStatus::Idle.bits() | SessionStatus::IsRead.bits() | SessionStatus::IsArchived.bits(),
            "the row's archive state shows through the parked answer"
        );
    }

    #[test]
    fn a_live_fold_and_a_parked_row_share_the_table() {
        let folded = manox_ahp::channels::session::initial("manox", None, None);
        let mut folded_archived = folded.clone();
        folded_archived.status = SessionStatus::IsArchived.bits();
        let base = row(false, false, false);
        assert_eq!(
            row_status_bits(Some(&folded), &base, true, true),
            SessionStatus::Idle.bits()
                | SessionStatus::InProgress.bits()
                | SessionStatus::InputNeeded.bits()
                | SessionStatus::IsRead.bits(),
            "the live facts ride the same table"
        );
        assert_eq!(
            row_status_bits(Some(&folded_archived), &base, false, false),
            SessionStatus::Idle.bits() | SessionStatus::IsRead.bits() | SessionStatus::IsArchived.bits(),
            "the fold's archive bit joins the row's"
        );
    }
}
