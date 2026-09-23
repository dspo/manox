//! Inbound message dispatch: `(method, params.channel)` routing, per AHP's
//! requirement that a peer can route any message from those two facts alone.
//!
//! Requests answer with a result or a stable-coded error; notifications are
//! fire-and-forget (`dispatchAction` answers through the echoed action envelope,
//! `unsubscribe` not at all). Unknown methods are refused for requests and
//! dropped for notifications, which is what lets a newer client talk to this
//! host without breaking.

use std::sync::Arc;

use ahp_types::actions::{ActionOrigin, SessionChatAddedAction, StateAction};
use ahp_types::commands::{
    CompletionsParams, CompletionsResult, CreateChatParams, CreateSessionParams,
    DispatchActionParams, DisposeChatParams, DisposeSessionParams, FetchTurnsParams,
    InitializeParams, InitializeResult, ListSessionsParams, ListSessionsResult, ReconnectResult,
    ReconnectSnapshotResult, ResolveSessionConfigParams, ResolveSessionConfigResult,
    ResourceDeleteParams, ResourceListParams, ResourceReadParams, ResourceWriteParams,
    SubscribeParams, SubscribeResult, UnsubscribeParams,
};
use ahp_types::messages::{JsonRpcMessage, JsonRpcNotification, JsonRpcRequest};
use ahp_types::state::Snapshot;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::channels::{self, Channel, chat, root, session};
use crate::connection::Conn;
use crate::error::HostError;
use crate::ext;
use crate::host::Inner;
use crate::wire;

/// Handle one inbound message.
pub(crate) async fn handle(inner: &Arc<Inner>, conn: &Arc<Conn>, msg: JsonRpcMessage) {
    match msg {
        JsonRpcMessage::Request(request) => {
            let id = request.id;
            match dispatch_request(inner, conn, &request).await {
                Ok(result) => conn.send(wire::success(id, result)),
                Err(err) => {
                    tracing::debug!(method = %request.method, "request failed: {err}");
                    conn.send(wire::error(
                        id,
                        ahp_types::messages::JsonRpcError::from(err),
                    ));
                }
            }
        }
        JsonRpcMessage::Notification(note) => dispatch_notification(inner, conn, &note).await,
        JsonRpcMessage::SuccessResponse(response) => {
            conn.complete_waiter(crate::jsonrpc::MsgId(response.id), Ok(response.result));
        }
        JsonRpcMessage::ErrorResponse(response) => {
            conn.complete_waiter(crate::jsonrpc::MsgId(response.id), Err(response.error));
        }
    }
}

async fn dispatch_request(
    inner: &Arc<Inner>,
    conn: &Arc<Conn>,
    request: &JsonRpcRequest,
) -> Result<Value, HostError> {
    let params = request.params.clone().unwrap_or(Value::Null);
    // The declared surface is the gate: an unknown *or* a declined method is
    // answered `MethodNotFound`, which tells a client the capability is absent
    // rather than empty.
    let Some(command) = crate::command::Command::of_method(&request.method) else {
        return Err(HostError::MethodNotFound(request.method.clone()));
    };
    if command.intent() == crate::command::CommandIntent::Declined {
        return Err(HostError::MethodNotFound(request.method.clone()));
    }
    match request.method.as_str() {
        "initialize" => initialize(inner, conn, params).await,
        "ping" => Ok(Value::Null),
        "subscribe" => subscribe(inner, conn, params).await,
        "reconnect" => reconnect(inner, conn, params).await,
        "listSessions" => list_sessions(inner, params),
        // The reference client resolves the session configuration *before* it
        // creates a session, and pages `completions` while the user types.
        // Answering `MethodNotFound` to either would stop it before it ever
        // reaches `createSession`, so both answer with the honest empty shape
        // (the plan's "宁可回空也不回 MethodNotFound" decision): a host that
        // advertises no pre-creation config, and a host with no completion
        // provider.
        "resolveSessionConfig" => resolve_session_config(params),
        "completions" => completions(params),
        "createSession" => create_session(inner, conn, params).await,
        "disposeSession" => dispose_session(inner, params),
        "createChat" => create_chat(inner, conn, params).await,
        "disposeChat" => dispose_chat(inner, params),
        "fetchTurns" => fetch_turns(inner, params),
        "resourceRead" => resource_read(inner, params),
        "resourceWrite" => resource_write(inner, params),
        "resourceList" => resource_list(inner, params),
        "resourceDelete" => resource_delete(inner, params),
        other if other.starts_with("x-manox/") => inner.backend.extension(other, &params),
        // The table and this match must agree: a name that resolves to a command
        // but reaches here is our own bug, so it surfaces as such instead of
        // masquerading as an unknown method.
        other => Err(HostError::Backend(format!(
            "command {other} is declared but not matched"
        ))),
    }
}

async fn dispatch_notification(inner: &Arc<Inner>, conn: &Arc<Conn>, note: &JsonRpcNotification) {
    match note.method.as_str() {
        "unsubscribe" => {
            let params = note.params.clone().unwrap_or(Value::Null);
            match parse_params::<UnsubscribeParams>(params) {
                Ok(params) => conn.unsubscribe(&params.channel),
                Err(err) => tracing::debug!("malformed unsubscribe: {err}"),
            }
        }
        wire::DISPATCH_ACTION_METHOD => {
            let params = note.params.clone().unwrap_or(Value::Null);
            match parse_params::<DispatchActionParams>(params) {
                Ok(params) => dispatch_action(inner, conn, params),
                Err(err) => tracing::debug!("malformed dispatchAction: {err}"),
            }
        }
        // Unknown notifications are ignored (never fatal): a newer client's
        // optional chatter must not break an older host.
        other => tracing::debug!(method = other, "ignoring unknown notification"),
    }
}

/// The write path: acceptance table → reducer → runtime side effects → echo.
///
/// AHP has no write receipt: the client learns the outcome from the echoed
/// action envelope (with its `serverSeq`) or from `rejectionReason`.
fn dispatch_action(inner: &Arc<Inner>, conn: &Arc<Conn>, params: DispatchActionParams) {
    let origin = ActionOrigin {
        client_id: conn.client_id().unwrap_or_default(),
        client_seq: params.client_seq,
    };
    let uri = params.channel.as_str();

    if !conn.is_subscribed(uri) {
        inner.reject(
            uri,
            params.action,
            Some(origin),
            "not subscribed to the action's channel".to_string(),
        );
        return;
    }
    let tag = wire::action_tag(&params.action);
    if !ext::accepts_action(uri, &tag) {
        inner.reject(
            uri,
            params.action,
            Some(origin),
            format!("action not accepted by this host: {tag}"),
        );
        return;
    }
    let Some(channel) = channels::parse(uri) else {
        inner.reject(
            uri,
            params.action,
            Some(origin),
            "unknown channel scheme".to_string(),
        );
        return;
    };
    match &channel {
        Channel::Session(id) => {
            if let Err(err) = inner.ensure_session(id) {
                inner.reject(uri, params.action, Some(origin), err.message());
                return;
            }
        }
        Channel::Chat(id) => {
            if let Err(err) = inner.ensure_chat(id) {
                inner.reject(uri, params.action, Some(origin), err.message());
                return;
            }
        }
        Channel::Terminal(id) => {
            if let Err(err) = inner.ensure_terminal(id) {
                inner.reject(uri, params.action, Some(origin), err.message());
                return;
            }
        }
        Channel::Root | Channel::Extension(_) => {}
    }

    match inner.backend.dispatch(uri, &params.action, &origin) {
        crate::backend::DispatchOutcome::Rejected(reason) => {
            inner.reject(uri, params.action, Some(origin), reason);
        }
        crate::backend::DispatchOutcome::Accepted | crate::backend::DispatchOutcome::Ignored => {
            inner.publish(uri, params.action, Some(origin));
        }
    }
}

async fn initialize(
    inner: &Arc<Inner>,
    conn: &Arc<Conn>,
    params: Value,
) -> Result<Value, HostError> {
    // The reference client omits `channel` on connection-level commands in some
    // builds; the root channel is what those commands mean either way, so a
    // missing field is defaulted rather than refused (interoperability over
    // schema literalism — a *wrong* channel is still rejected).
    let params: InitializeParams = parse_params(params)?;
    if !params.channel.is_empty() && params.channel != root::URI {
        return Err(HostError::InvalidParams(
            "initialize targets ahp-root://".to_string(),
        ));
    }
    let chosen = crate::jsonrpc::version::negotiate(&params.protocol_versions)?;

    inner.reseat(conn, &params.client_id);
    conn.set_identity(params.client_id.clone(), params.locale.clone());

    let mut snapshots = Vec::new();
    for uri in params.initial_subscriptions.clone().unwrap_or_default() {
        if let Some(snapshot) = subscribe_uri(inner, conn, &uri)? {
            snapshots.push(shape_snapshot(snapshot, None));
        }
    }

    let result = InitializeResult {
        protocol_version: chosen,
        server_seq: inner.watermark(),
        server_info: Some(ahp_types::commands::Implementation {
            name: "manox-ahp".to_string(),
            version: Some(env!("CARGO_PKG_VERSION").to_string()),
            title: Some("manox".to_string()),
        }),
        meta: Some(declaration_meta()),
        snapshots,
        default_directory: None,
        completion_trigger_characters: Some(vec!["@".to_string(), "/".to_string()]),
        terminal_command_prefix: Some("!".to_string()),
        telemetry: None,
        automations: None,
    };
    to_value(result)
}

async fn subscribe(
    inner: &Arc<Inner>,
    conn: &Arc<Conn>,
    params: Value,
) -> Result<Value, HostError> {
    let params: SubscribeParams = parse_params(params)?;
    let view_turns = params.view.as_ref().and_then(|view| view.turns);
    let snapshot = subscribe_uri(inner, conn, &params.channel)?
        .map(|snapshot| shape_snapshot(snapshot, view_turns));
    to_value(SubscribeResult { snapshot })
}

/// Bound a chat snapshot to a turn tail (see [`crate::channels::chat::tail_view`]).
///
/// AHP would have a host return every retained turn when the client omits
/// `view.turns`; a journal-backed session makes that frame unbounded, so this
/// host caps the tail and hands out the paging cursor instead. A client that
/// pages (`fetchTurns`) loses nothing; one that does not simply sees the tail.
fn shape_snapshot(snapshot: Snapshot, view_turns: Option<i64>) -> Snapshot {
    const DEFAULT_TAIL_TURNS: usize = 40;
    let ahp_types::state::SnapshotState::Chat(state) = &snapshot.state else {
        return snapshot;
    };
    let cut = crate::channels::chat::tail_view(state, view_turns, DEFAULT_TAIL_TURNS);
    Snapshot {
        resource: snapshot.resource,
        state: ahp_types::state::SnapshotState::Chat(Box::new(cut)),
        from_seq: snapshot.from_seq,
    }
}

/// Subscribe one channel and answer its snapshot (or `None` for stateless
/// channels, whose baseline is pushed as an extension action instead).
fn subscribe_uri(
    inner: &Arc<Inner>,
    conn: &Arc<Conn>,
    uri: &str,
) -> Result<Option<Snapshot>, HostError> {
    let channel = channels::parse(uri)
        .ok_or_else(|| HostError::InvalidParams(format!("unknown channel scheme: {uri}")))?;
    match &channel {
        Channel::Root => {}
        Channel::Session(id) => inner.ensure_session(id)?,
        Channel::Chat(id) => {
            inner.ensure_chat(id)?;
        }
        Channel::Terminal(id) => inner.ensure_terminal(id)?,
        Channel::Extension(_) => {}
    }
    conn.subscribe(uri);
    let snapshot = inner.snapshot(&channel);
    if snapshot.is_none()
        && !channel.is_state_bearing()
        && let Some((method, params)) = inner.backend.extension_baseline(uri)
    {
        conn.send(wire::notification(&method, params));
    }
    Ok(snapshot)
}

/// The snapshot leg of reconnection.
///
/// AHP lets a host answer `reconnect` with either replayed actions or fresh
/// snapshots. manox always has a snapshot: the durable journal folds
/// deterministically, so a snapshot is never stale, and a bounded replay buffer
/// becomes an optimisation (W5) instead of a correctness requirement.
/// Subscriptions the host cannot resume simply do not appear in the answer.
async fn reconnect(
    inner: &Arc<Inner>,
    conn: &Arc<Conn>,
    params: Value,
) -> Result<Value, HostError> {
    // Parsed by hand, not through `ReconnectParams`: the reference client sends
    // `reconnect` as a connection's **first** message with only `clientId` and
    // `subscriptions` (no `channel`), and refusing it costs the whole handshake —
    // the client then reports "Unable to connect to remote agent host".
    let client_id = params
        .get("clientId")
        .and_then(Value::as_str)
        .ok_or_else(|| HostError::InvalidParams("reconnect needs clientId".to_string()))?
        .to_string();
    let subscriptions: Vec<String> = params
        .get("subscriptions")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    inner.reseat(conn, &client_id);
    conn.set_identity(client_id.clone(), None);

    let mut snapshots = Vec::new();
    for uri in &subscriptions {
        if let Ok(Some(snapshot)) = subscribe_uri(inner, conn, uri) {
            snapshots.push(shape_snapshot(snapshot, None));
        }
    }
    tracing::info!(
        client_id = %client_id,
        last_seen = params.get("lastSeenServerSeq").and_then(|value| value.as_i64()).unwrap_or_default(),
        subscriptions = subscriptions.len(),
        "reconnect answered with snapshots"
    );
    to_value(ReconnectResult::Snapshot(ReconnectSnapshotResult {
        snapshots,
    }))
}

/// An empty-but-valid session configuration: no pre-creation options, no
/// resolved values. Model choice and thinking level reach clients through
/// `AgentInfo.models[].configSchema` instead, because they are per-model.
fn resolve_session_config(params: Value) -> Result<Value, HostError> {
    let _params: ResolveSessionConfigParams = parse_params(params)?;
    to_value(ResolveSessionConfigResult {
        schema: ahp_types::state::SessionConfigSchema {
            r#type: "object".to_string(),
            properties: std::collections::HashMap::new(),
            required: None,
        },
        values: ahp_types::common::JsonObject::new(),
    })
}

/// No completion provider: an empty item list, which is what a client showing
/// an empty picker expects.
fn completions(params: Value) -> Result<Value, HostError> {
    let _params: CompletionsParams = parse_params(params)?;
    to_value(CompletionsResult { items: Vec::new() })
}

fn list_sessions(inner: &Arc<Inner>, params: Value) -> Result<Value, HostError> {
    const DEFAULT_PAGE: usize = 50;
    const MAX_PAGE: usize = 500;

    let params: ListSessionsParams = parse_params(params)?;
    if params.channel != root::URI {
        return Err(HostError::InvalidParams(
            "listSessions targets ahp-root://".to_string(),
        ));
    }
    let all = inner.backend.list_sessions();
    let start = match params.cursor.as_deref() {
        None => 0,
        Some(cursor) => cursor
            .parse::<usize>()
            .map_err(|_| HostError::InvalidParams("unrecognised cursor".to_string()))?,
    };
    let limit = match params.limit {
        None => DEFAULT_PAGE,
        Some(limit) if limit < 0 => {
            return Err(HostError::InvalidParams("negative limit".to_string()));
        }
        Some(limit) => (limit as usize).min(MAX_PAGE),
    };
    let items: Vec<_> = all.iter().skip(start).take(limit).cloned().collect();
    let next = start + items.len();
    let next_cursor = if next < all.len() {
        Some(next.to_string())
    } else {
        None
    };
    to_value(ListSessionsResult { items, next_cursor })
}

async fn create_session(
    inner: &Arc<Inner>,
    _conn: &Arc<Conn>,
    params: Value,
) -> Result<Value, HostError> {
    let params: CreateSessionParams = parse_params(tolerate_missing_active_client_tools(params))?;
    let session_id = session::id(&params.channel)
        .ok_or_else(|| HostError::InvalidParams("createSession channel".to_string()))?
        .to_string();
    if inner.store.read().session(&session_id).is_some() {
        return Err(HostError::SessionAlreadyExists(session_id));
    }
    inner.backend.create_session(&session_id, &params)?;
    inner.ensure_session(&session_id)?;
    if let Some(summary) = inner.backend.session_summary(&session_id) {
        inner.session_added(summary);
    }
    Ok(Value::Null)
}

/// Default `activeClient.tools` to an empty list when a client omits it.
///
/// `SessionActiveClient.tools` is **required** by the pinned `ahp-types` 0.9.0
/// (no `#[serde(default)]`) and equally required by the specification's own
/// TypeScript type (`state.d.ts`: `tools: ToolDefinition[]`) — but the
/// specification's own TypeScript **client** does not send it when it
/// constructs `createSession` (the command travels its generic `CommandMap`
/// path, whose `activeClient` is assembled with `clientId` alone). So the types
/// demand a field the reference client never fills, and a strict host answers
/// `-32602` to a spec-conformant client that is doing nothing wrong.
///
/// This follows the same ruling as the other reference-client leniencies here
/// (the omitted `channel` on connection-level commands): interoperability over
/// schema literalism. A *malformed* `tools` is still rejected — only a missing
/// one is defaulted, and a client that means to contribute tools still must
/// list them.
///
/// The workaround is self-invalidating: if an upgrade makes the field optional
/// upstream, the JSON rewrite below stops being reachable and this function
/// should be deleted. `tolerated_upstream_gaps()` counts it.
fn tolerate_missing_active_client_tools(mut params: Value) -> Value {
    if let Some(client) = params
        .get_mut("activeClient")
        .and_then(Value::as_object_mut)
        && !client.contains_key("tools")
    {
        client.insert("tools".to_string(), Value::Array(Vec::new()));
    }
    params
}

/// How many upstream-gap workarounds this host currently carries.
///
/// Each entry is a place where the pinned upstream types and the reference
/// client disagree and we bridge the difference. The count exists so an upgrade
/// cannot silently leave one behind: a test asserts the expected number and
/// names each workaround, so when upstream closes a gap this fails and points at
/// the function to delete (the "self-invalidating guard" pattern pi-ahp uses).
///
/// Current entries:
/// 1. [`tolerate_missing_active_client_tools`] — `createSession` with an
///    `activeClient` that omits the upstream-required `tools`.
pub fn tolerated_upstream_gaps() -> usize {
    1
}

fn dispose_session(inner: &Arc<Inner>, params: Value) -> Result<Value, HostError> {
    let params: DisposeSessionParams = parse_params(params)?;
    let session_id = session::id(&params.channel)
        .ok_or_else(|| HostError::InvalidParams("disposeSession channel".to_string()))?
        .to_string();
    inner.backend.dispose_session(&session_id)?;
    inner.store.write().remove_session(&session_id);
    inner.session_removed(&session_id);
    Ok(Value::Null)
}

async fn create_chat(
    inner: &Arc<Inner>,
    _conn: &Arc<Conn>,
    params: Value,
) -> Result<Value, HostError> {
    let params: CreateChatParams = parse_params(params)?;
    let session_id = session::id(&params.channel)
        .ok_or_else(|| HostError::InvalidParams("createChat channel".to_string()))?
        .to_string();
    let chat_id = chat::id(&params.chat)
        .ok_or_else(|| HostError::InvalidParams("createChat chat".to_string()))?
        .to_string();
    inner.ensure_session(&session_id)?;
    if inner.store.read().chat(&chat_id).is_some() {
        return Err(HostError::AlreadyExists(chat::uri(&chat_id)));
    }
    inner.backend.create_chat(&session_id, &chat_id, &params)?;
    let state = inner
        .backend
        .chat_state(&chat_id)
        .ok_or_else(|| HostError::NotFound(chat::uri(&chat_id)))?;
    let summary = chat::summary(&state);
    inner
        .store
        .write()
        .insert_chat(&session_id, &chat_id, state);
    // The session's catalog is the client's entry point to the new chat, so the
    // host mirrors it there as AHP's `session/chatAdded` action.
    inner.publish(
        &session::uri(&session_id),
        StateAction::SessionChatAdded(SessionChatAddedAction { summary }),
        None,
    );
    Ok(Value::Null)
}

fn dispose_chat(inner: &Arc<Inner>, params: Value) -> Result<Value, HostError> {
    let params: DisposeChatParams = parse_params(params)?;
    let chat_id = chat::id(&params.channel)
        .ok_or_else(|| HostError::InvalidParams("disposeChat channel".to_string()))?
        .to_string();
    let session_id = inner
        .store
        .read()
        .chat_session(&chat_id)
        .map(str::to_string);
    inner.backend.dispose_chat(&chat_id)?;
    if let Some(session_id) = session_id {
        inner.publish(
            &session::uri(&session_id),
            StateAction::SessionChatRemoved(ahp_types::actions::SessionChatRemovedAction {
                chat: chat::uri(&chat_id),
            }),
            None,
        );
    }
    inner.store.write().remove_chat(&chat_id);
    Ok(Value::Null)
}

fn fetch_turns(inner: &Arc<Inner>, params: Value) -> Result<Value, HostError> {
    let params: FetchTurnsParams = parse_params(params)?;
    let chat_id = chat::id(&params.channel)
        .ok_or_else(|| HostError::InvalidParams("fetchTurns channel".to_string()))?
        .to_string();
    inner.ensure_chat(&chat_id)?;
    let (turns, turns_next_cursor) =
        inner
            .backend
            .fetch_turns_page(&chat_id, params.cursor.as_deref(), None)?;
    inner.publish(
        &params.channel,
        StateAction::ChatTurnsLoaded(ahp_types::actions::ChatTurnsLoadedAction {
            turns,
            turns_next_cursor,
        }),
        None,
    );
    to_value(ahp_types::commands::FetchTurnsResult {})
}

fn resource_read(inner: &Arc<Inner>, params: Value) -> Result<Value, HostError> {
    let params: ResourceReadParams = parse_params(params)?;
    let plane = inner
        .backend
        .resources()
        .ok_or_else(|| HostError::Unimplemented("resourceRead".to_string()))?;
    to_value(plane.read(&params)?)
}

fn resource_write(inner: &Arc<Inner>, params: Value) -> Result<Value, HostError> {
    let params: ResourceWriteParams = parse_params(params)?;
    let plane = inner
        .backend
        .resources()
        .ok_or_else(|| HostError::Unimplemented("resourceWrite".to_string()))?;
    plane.write(&params)?;
    Ok(Value::Null)
}

fn resource_list(inner: &Arc<Inner>, params: Value) -> Result<Value, HostError> {
    let params: ResourceListParams = parse_params(params)?;
    let plane = inner
        .backend
        .resources()
        .ok_or_else(|| HostError::Unimplemented("resourceList".to_string()))?;
    to_value(plane.list(&params.uri)?)
}

fn resource_delete(inner: &Arc<Inner>, params: Value) -> Result<Value, HostError> {
    let params: ResourceDeleteParams = parse_params(params)?;
    let plane = inner
        .backend
        .resources()
        .ok_or_else(|| HostError::Unimplemented("resourceDelete".to_string()))?;
    plane.delete(&params.uri)?;
    Ok(Value::Null)
}

/// The `_meta` payload carried on `initialize` results: the extension
/// declaration, so a client knows exactly which private surface this host
/// serves before it opens a channel.
fn declaration_meta() -> ahp_types::common::JsonObject {
    let mut meta = ahp_types::common::JsonObject::new();
    meta.insert(ext::META_KEY.to_string(), ext::declaration());
    meta
}

/// Parse command params, converting serde failures into `-32602`.
fn parse_params<T: DeserializeOwned>(value: Value) -> Result<T, HostError> {
    serde_json::from_value(value).map_err(|err| HostError::InvalidParams(err.to_string()))
}

/// Serialize a result, converting the (impossible-by-construction) failure into
/// an internal error rather than panicking.
fn to_value<T: serde::Serialize>(value: T) -> Result<Value, HostError> {
    serde_json::to_value(value).map_err(|err| HostError::Backend(err.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The self-invalidating guard: when an upstream upgrade closes a gap, this
    /// fails and names the workaround to delete. A count that only ever grows is
    /// how a compatibility shim outlives its reason.
    #[test]
    fn upstream_gap_workarounds_are_counted_and_named() {
        assert_eq!(
            tolerated_upstream_gaps(),
            1,
            "if upstream made `activeClient.tools` optional (or the reference \
             client started sending it), delete \
             `tolerate_missing_active_client_tools` and lower this count"
        );
    }

    /// Only a *missing* `tools` is defaulted; a malformed one still fails, so
    /// the tolerance cannot swallow a real client bug.
    #[test]
    fn a_missing_tools_is_defaulted_but_a_malformed_one_is_not() {
        let filled = tolerate_missing_active_client_tools(
            serde_json::json!({"channel": "ahp-root://", "activeClient": {"clientId": "c"}}),
        );
        assert_eq!(filled["activeClient"]["tools"], serde_json::json!([]));

        // A client that supplies tools keeps exactly what it supplied.
        let kept = tolerate_missing_active_client_tools(serde_json::json!({
            "channel": "ahp-root://",
            "activeClient": {"clientId": "c", "tools": [{"name": "t"}]},
        }));
        assert_eq!(kept["activeClient"]["tools"][0]["name"], "t");

        // A malformed `tools` is not repaired into an empty list.
        let malformed = tolerate_missing_active_client_tools(serde_json::json!({
            "channel": "ahp-root://",
            "activeClient": {"clientId": "c", "tools": "not-a-list"},
        }));
        assert_eq!(malformed["activeClient"]["tools"], "not-a-list");

        // No activeClient at all is left alone (nothing to default).
        let none =
            tolerate_missing_active_client_tools(serde_json::json!({"channel": "ahp-root://"}));
        assert!(none.get("activeClient").is_none());
    }
}
