//! napi-rs bindings exposing the manox agent core to a TypeScript host.
//!
//! The binding is a **raw transport bridge**, nothing more: it hands the
//! TypeScript side one end of an in-process AHP connection and moves complete
//! JSON-RPC messages across it in both directions. The protocol itself —
//! channels, subscriptions, request correlation — belongs to the AHP client
//! SDK on the TypeScript side, and the host end is served by the
//! process-singleton AHP runtime on this side.
//!
//! Nothing here parses or interprets a message. A method or action this build
//! does not serve is refused by the host with the protocol's own error, and a
//! frame vocabulary that grows later needs no change in this file.

#[macro_use]
extern crate napi_derive;

use std::sync::Mutex;

use ahp_types::messages::JsonRpcMessage;
use napi::bindgen_prelude::*;
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};

/// The live connection to the in-process AHP host.
static CONN: Mutex<Option<ConnectionState>> = Mutex::new(None);

struct ConnectionState {
    /// Outbound messages, as protocol text, for the host-side sink task.
    outgoing: async_channel::Sender<String>,
    /// The pump thread handle. Dropped on shutdown.
    _pump: std::thread::JoinHandle<()>,
    /// Keeps the process-singleton runtime (and therefore the session store)
    /// alive for as long as the binding is started.
    _runtime: std::sync::Arc<manox_ahp_runtime::ahp::runtime::AhpRuntime>,
}

fn conn_slot() -> std::sync::MutexGuard<'static, Option<ConnectionState>> {
    CONN.lock().unwrap_or_else(|e| e.into_inner())
}

/// Smoke-test export: verifies the native module loads and links the agent
/// dependency graph.
#[napi]
pub fn ping() -> String {
    "pong".to_string()
}

/// Start the agent runtime and connect one in-process AHP transport.
///
/// `event_cb` receives one JSON string per host→client message, on the Node
/// main thread, scheduled from the pump thread through a threadsafe function.
/// The caller is responsible for the protocol: it sends `initialize` and every
/// subsequent request or notification through [`send_command`].
///
/// `client_id` names the host identity to pin. The AHP handshake carries the
/// client id on each request, so this only seeds the agent host's own identity
/// (the VS Code host vs. the desktop one) — it is not a protocol-level login.
#[napi]
pub fn start(client_id: String, event_cb: JsFunction) -> Result<()> {
    let mut slot = conn_slot();
    if slot.is_some() {
        return Err(napi::Error::from_reason("actor already started"));
    }

    // Pin the host identity and initialize the agent runtime.
    let _ = client_id;
    manox_agent::host::set_host(manox_agent::host::Host::Vscode);
    manox_agent::init();

    let tsfn: ThreadsafeFunction<String> =
        event_cb.create_threadsafe_function(0, |ctx| Ok(vec![ctx.value]))?;

    let cwd = std::env::current_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("/"))
        .to_string_lossy()
        .to_string();

    // The gateway OWNS the session store, so it is constructed first: it
    // installs the runtime builder the process-singleton host is built from,
    // and registers the capability + embedder-tool providers the engine
    // consults when it assembles a session's tools.
    let _server = manox_session_core::agent_server::global(std::path::PathBuf::from(&cwd));
    let runtime = manox_ahp_runtime::ahp::runtime::runtime(std::path::PathBuf::from(&cwd));

    // One in-process pair. Both queues are unbounded: a bounded pair filled in
    // both directions would deadlock the Node main thread against the host.
    let (to_host_tx, to_host_rx) = async_channel::unbounded::<String>();
    let (to_client_tx, to_client_rx) = async_channel::unbounded::<String>();

    let host = manox_ahp::transport::HostTransport::new(
        Box::new(TextSink { tx: to_client_tx }),
        Box::new(TextSource { rx: to_host_rx }),
    );
    runtime.accept(host);

    let pump = std::thread::Builder::new()
        .name("manox-napi-pump".into())
        .spawn(move || {
            while let Ok(message) = to_client_rx.recv_blocking() {
                if !matches!(
                    tsfn.call(Ok(message), ThreadsafeFunctionCallMode::NonBlocking),
                    napi::Status::Ok
                ) {
                    break;
                }
            }
        })
        .map_err(|e| napi::Error::from_reason(format!("failed to spawn pump thread: {e}")))?;

    *slot = Some(ConnectionState {
        outgoing: to_host_tx,
        _pump: pump,
        _runtime: runtime,
    });
    Ok(())
}

/// Deliver one serialized AHP request or notification to the host.
#[napi]
pub fn send_command(command: String) -> Result<()> {
    match conn_slot().as_ref() {
        Some(state) => state
            .outgoing
            .try_send(command)
            .map_err(|e| napi::Error::from_reason(format!("host connection closed: {e}"))),
        None => Err(napi::Error::from_reason(
            "actor not started; call start(callback) first",
        )),
    }
}

/// Tear down the agent connection and release the event callback. Safe to call
/// when the actor is not started; intended for host `deactivate`.
#[napi]
pub fn shutdown() {
    if let Some(state) = conn_slot().take() {
        // Closing the queue ends the host's source half, which ends the
        // connection; the pump thread then sees its own queue close.
        state.outgoing.close();
    }
}

/// The host's writing half: protocol text out to the TypeScript side.
struct TextSink {
    tx: async_channel::Sender<String>,
}

impl manox_ahp::transport::Sink for TextSink {
    fn send(
        &mut self,
        msg: JsonRpcMessage,
    ) -> manox_ahp::transport::TransportFuture<
        '_,
        std::result::Result<(), manox_ahp::transport::TransportError>,
    > {
        Box::pin(async move {
            let text = manox_ahp::wire::to_text(&msg);
            self.tx
                .send(text)
                .await
                .map_err(|_| manox_ahp::transport::TransportError("client closed".into()))
        })
    }
}

/// The host's reading half: protocol text in from the TypeScript side.
struct TextSource {
    rx: async_channel::Receiver<String>,
}

impl manox_ahp::transport::Source for TextSource {
    fn recv(
        &mut self,
    ) -> manox_ahp::transport::TransportFuture<
        '_,
        std::result::Result<Option<JsonRpcMessage>, manox_ahp::transport::TransportError>,
    > {
        Box::pin(async move {
            let Some(text) = self.rx.recv().await.ok() else {
                return Ok(None);
            };
            manox_ahp::wire::parse_text(&text)
                .map(Some)
                .map_err(|err| manox_ahp::transport::TransportError(err.message))
        })
    }
}

// ── The object store: typed bindings ───────────────────────────────────────────
//
// The raw AHP bridge above moves protocol frames; these exports are the
// object API — projects and conversations as durable rows, managed by
// direct synchronous calls (the same setters the desktop app uses
// in-process). A TypeScript host needs no protocol vocabulary for user
// will: create conversations, attach them to projects, pick models and
// permission modes, persist drafts, and read the lists.

/// One project row.
#[napi(object)]
pub struct JsProject {
    pub id: String,
    pub path: String,
    pub alias: Option<String>,
    pub position: i64,
}

/// One conversation row.
#[napi(object)]
pub struct JsConversation {
    pub id: String,
    pub project_id: Option<String>,
    pub model_provider: Option<String>,
    pub model_id: Option<String>,
    pub permission_mode: Option<String>,
    pub reasoning_effort: Option<String>,
    pub draft: Option<String>,
    pub title: Option<String>,
    pub pinned: bool,
    pub archived: bool,
    pub unread: bool,
    pub errored: bool,
    pub created_at: i64,
    pub updated_at: i64,
    /// Unix seconds of the last human-authored message; `null` until the
    /// conversation actually happens (the draft-refill window).
    pub last_human_message_at: Option<i64>,
    pub config_version: i64,
}

fn objects() -> manox_agent::object_store::ObjectStore {
    manox_agent::object_store::ObjectStore
}

fn js_project(row: manox_agent::object_store::ProjectRow) -> JsProject {
    JsProject {
        id: row.id,
        path: row.path,
        alias: row.alias,
        position: row.position,
    }
}

fn js_conversation(row: manox_agent::object_store::ConversationRow) -> JsConversation {
    JsConversation {
        id: row.id,
        project_id: row.project_id,
        model_provider: row.model_provider,
        model_id: row.model_id,
        permission_mode: row.permission_mode,
        reasoning_effort: row.reasoning_effort,
        draft: row.draft,
        title: row.title,
        pinned: row.pinned,
        archived: row.archived,
        unread: row.unread,
        errored: row.errored,
        created_at: row.created_at,
        updated_at: row.updated_at,
        last_human_message_at: row.last_human_message_at,
        config_version: row.config_version,
    }
}

/// Register a project path (idempotent); newly registered projects take the
/// head position. Returns the project id.
#[napi]
pub fn object_register_project(path: String) -> Result<String> {
    ensure_objects()?;
    Ok(objects().register_project(&path))
}

/// Set (or clear) a project's display alias.
#[napi]
pub fn object_set_project_alias(path: String, alias: Option<String>) -> Result<bool> {
    ensure_objects()?;
    Ok(objects().set_project_alias(&path, alias.as_deref()))
}

/// Move a project before another in the folder order (`null` = tail).
#[napi]
pub fn object_move_project(path: String, before: Option<String>) -> Result<bool> {
    ensure_objects()?;
    Ok(objects().move_project(&path, before.as_deref()))
}

/// All projects in committed folder order.
#[napi]
pub fn object_list_projects() -> Result<Vec<JsProject>> {
    ensure_objects()?;
    Ok(objects()
        .list_projects()
        .into_iter()
        .map(js_project)
        .collect())
}

/// Create a conversation row (idempotent on the id).
#[napi]
pub fn object_create_conversation(id: String, project_path: Option<String>) -> Result<bool> {
    ensure_objects()?;
    Ok(objects().create_conversation(&id, project_path.as_deref()))
}

/// Attach a conversation to a project. Belonging only — no cwd ever moves.
#[napi]
pub fn object_attach(id: String, project_path: String) -> Result<bool> {
    ensure_objects()?;
    Ok(objects().attach(&id, &project_path))
}

/// Detach: the conversation becomes project-less.
#[napi]
pub fn object_detach(id: String) -> Result<()> {
    ensure_objects()?;
    objects().detach(&id);
    Ok(())
}

/// Set the model pick (bumps the config version the engine watches).
#[napi]
pub fn object_set_model(
    id: String,
    provider: Option<String>,
    model_id: Option<String>,
) -> Result<()> {
    ensure_objects()?;
    objects().set_model(&id, provider.as_deref(), model_id.as_deref());
    Ok(())
}

/// Set the permission mode pick (read before every tool execution).
#[napi]
pub fn object_set_permission_mode(id: String, mode: Option<String>) -> Result<()> {
    ensure_objects()?;
    objects().set_permission_mode(&id, mode.as_deref());
    Ok(())
}

/// Set the reasoning effort pick.
#[napi]
pub fn object_set_reasoning_effort(id: String, effort: Option<String>) -> Result<()> {
    ensure_objects()?;
    objects().set_reasoning_effort(&id, effort.as_deref());
    Ok(())
}

/// Persist the composer draft (pure UI state; no config bump).
#[napi]
pub fn object_set_draft(id: String, draft: Option<String>) -> Result<()> {
    ensure_objects()?;
    objects().set_draft(&id, draft.as_deref());
    Ok(())
}

/// Pin or unpin.
#[napi]
pub fn object_set_pinned(id: String, pinned: bool) -> Result<()> {
    ensure_objects()?;
    objects().set_pinned(&id, pinned);
    Ok(())
}

/// Archive or unarchive.
#[napi]
pub fn object_set_archived(id: String, archived: bool) -> Result<()> {
    ensure_objects()?;
    objects().set_archived(&id, archived);
    Ok(())
}

/// Rename (the display title).
#[napi]
pub fn object_set_title(id: String, title: Option<String>) -> Result<()> {
    ensure_objects()?;
    objects().set_title(&id, title.as_deref());
    Ok(())
}

/// One conversation's row, if it exists.
#[napi]
pub fn object_get_conversation(id: String) -> Result<Option<JsConversation>> {
    ensure_objects()?;
    Ok(objects().conversation(&id).map(js_conversation))
}

/// The materialized conversations, newest human message first.
#[napi]
pub fn object_list_conversations() -> Result<Vec<JsConversation>> {
    ensure_objects()?;
    Ok(objects()
        .list_conversations()
        .into_iter()
        .map(js_conversation)
        .collect())
}

/// The most recent never-materialized conversation under a project (or
/// project-less): the draft-refill candidate.
#[napi]
pub fn object_latest_draft(project_path: Option<String>) -> Result<Option<JsConversation>> {
    ensure_objects()?;
    Ok(objects()
        .latest_draft(project_path.as_deref())
        .map(js_conversation))
}

/// The object store needs the process runtime initialized — the same
/// `manox_agent::init` the raw bridge runs at `start`. These bindings are
/// callable before any AHP connection exists, so they initialize lazily.
fn ensure_objects() -> Result<()> {
    if manox_agent::object_store::ObjectStore::try_global().is_none() {
        return Err(napi::Error::from_reason(
            "object store not initialized; call start() first",
        ));
    }
    Ok(())
}
