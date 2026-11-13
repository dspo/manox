//! Per-connection channel lanes: the connection reader is a demultiplexer.
//!
//! AHP orders messages **per channel** and says nothing about ordering across
//! channels, so a connection runs one FIFO lane per target channel. Order within
//! a channel is preserved — a `subscribe` and the `dispatchAction` that follows
//! it cannot be reordered — while a slow channel cannot delay another, which is
//! what keeps `createSession`, `ping` and the handshake out of a busy session's
//! queue.
//!
//! The reader never awaits application work and never blocks on a full lane: a
//! request that cannot be queued is answered [`crate::codes::X_MANOX_OVERLOAD`],
//! a notification is dropped with a warning, and the connection stays
//! responsive.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use ahp_types::messages::{JsonRpcError, JsonRpcMessage};
use serde_json::Value;

use crate::channels::root;
use crate::codes;
use crate::connection::Conn;
use crate::host::Inner;
use crate::wire;

/// How long one lane lets a handler run before answering its request itself.
///
/// A lane keeps its order by awaiting each handler, so a handler that never
/// returns would otherwise park that channel forever with the client waiting.
/// The deadline turns that into a protocol-visible
/// [`crate::codes::X_MANOX_TIMEOUT`]; the lane keeps its slot and the handler's
/// eventual reply is discarded (see [`Conn::reply_once`]). Generous on purpose:
/// it is a stuck-handler guard, not a per-command latency budget.
pub const DISPATCH_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

/// How many messages one channel's lane buffers before it reports overload.
///
/// The bound is the connection's per-channel memory ceiling: the reader either
/// accepts a message or answers it overloaded, and never waits for a lane to
/// drain.
pub const LANE_CAPACITY: usize = 256;

/// One message waiting in (or running from) a lane, stamped when the reader
/// accepted it: the request deadline covers queue time, not just handler time.
pub(crate) struct Queued {
    msg: JsonRpcMessage,
    at: tokio::time::Instant,
}

/// One connection's lane registry, owned by its reader task.
///
/// Dropping the registry drops every sender, which closes every lane channel and
/// ends its task: lanes live exactly as long as the connection's reader.
pub(crate) struct Lanes {
    inner: Arc<Inner>,
    conn: Arc<Conn>,
    lanes: Mutex<HashMap<String, async_channel::Sender<Queued>>>,
}

impl Lanes {
    /// A registry with no lanes; lanes open on first use.
    pub(crate) fn new(inner: Arc<Inner>, conn: Arc<Conn>) -> Self {
        Self {
            inner,
            conn,
            lanes: Mutex::new(HashMap::new()),
        }
    }

    /// Hand one message to its channel's lane without ever blocking.
    ///
    /// A request claims its one-answer slot HERE, before it can sit in a
    /// queue: a duplicate id sent while the first copy is parked must be
    /// dropped at the door, not executed twice once both dequeue.
    pub(crate) fn dispatch(&self, lane: String, msg: JsonRpcMessage) {
        if let JsonRpcMessage::Request(request) = &msg
            && !claim(&self.conn, request.id)
        {
            return;
        }
        let mut carry = Queued {
            msg,
            at: tokio::time::Instant::now(),
        };
        let mut retried = false;
        loop {
            let (tx, fresh) = self.sender(&lane);
            if let Some(rx) = fresh {
                self.spawn_lane(&lane, rx);
            }
            match tx.try_send(carry) {
                Ok(()) => return,
                Err(async_channel::TrySendError::Full(queued)) => {
                    self.overloaded(&lane, queued);
                    return;
                }
                Err(async_channel::TrySendError::Closed(queued)) => {
                    // The lane task died without the registry noticing. Retire
                    // the dead sender, open a fresh lane and retry once, so a
                    // dead lane cannot silently swallow a request.
                    self.retire(&lane, &tx);
                    if retried {
                        self.overloaded(&lane, queued);
                        return;
                    }
                    retried = true;
                    carry = queued;
                }
            }
        }
    }

    /// The lane's sender, opening the lane when it does not exist yet (or its
    /// task has ended). The returned receiver belongs to the new lane task.
    fn sender(
        &self,
        lane: &str,
    ) -> (
        async_channel::Sender<Queued>,
        Option<async_channel::Receiver<Queued>>,
    ) {
        let mut lanes = self.lock();
        if let Some(tx) = lanes.get(lane) {
            if !tx.is_closed() {
                return (tx.clone(), None);
            }
            lanes.remove(lane);
        }
        let (tx, rx) = async_channel::bounded(LANE_CAPACITY);
        lanes.insert(lane.to_string(), tx.clone());
        (tx, Some(rx))
    }

    /// Start the task that drains one lane, one handler at a time.
    fn spawn_lane(&self, lane: &str, rx: async_channel::Receiver<Queued>) {
        let inner = self.inner.clone();
        let conn = self.conn.clone();
        let lane = lane.to_string();
        tokio::spawn(async move {
            while let Ok(msg) = rx.recv().await {
                run_one(&inner, &conn, &lane, msg).await;
            }
        });
    }

    /// Forget a sender whose lane task ended.
    fn retire(&self, lane: &str, dead: &async_channel::Sender<Queued>) {
        let mut lanes = self.lock();
        if lanes.get(lane).is_some_and(|tx| tx.same_channel(dead)) {
            lanes.remove(lane);
        }
    }

    /// Report a message the lane could not take, without blocking the reader.
    ///
    /// The request already holds its claim (taken at dispatch), so the
    /// overload answer is its one reply and releases it.
    fn overloaded(&self, lane: &str, queued: Queued) {
        match queued.msg {
            JsonRpcMessage::Request(request) => {
                tracing::warn!(
                    connection = self.conn.id(),
                    lane = lane,
                    method = %request.method,
                    "channel lane is full; answering overload"
                );
                self.conn.reply_once(
                    request.id,
                    wire::error(
                        request.id,
                        JsonRpcError {
                            code: codes::X_MANOX_OVERLOAD,
                            message: format!(
                                "{} was not dispatched: the {lane} lane is full",
                                request.method
                            ),
                            data: None,
                        },
                    ),
                );
            }
            JsonRpcMessage::Notification(note) => tracing::warn!(
                connection = self.conn.id(),
                lane = lane,
                method = %note.method,
                "channel lane is full; dropping notification"
            ),
            other => tracing::warn!(
                connection = self.conn.id(),
                lane = lane,
                "channel lane is full; dropping {other:?}"
            ),
        }
    }

    /// The registry lock is held only across the map operations, and a poisoned
    /// lock is still usable: a panic elsewhere must not take the connection's
    /// lanes with it.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, async_channel::Sender<Queued>>> {
        self.lanes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// The lane a message belongs to: the channel it targets.
///
/// AHP's routing rule is `(method, params.channel)`, so the channel *is* the
/// unit of ordering. Creation is the exception that makes the demux useful:
/// `createChat` carries the *session* in `channel` but creates the chat URI, and
/// laning it on the session would put "create a chat" behind that session's
/// traffic. A creation method therefore lanes on the URI it creates. Messages
/// with no channel share the root lane.
pub(crate) fn target_channel(msg: &JsonRpcMessage) -> String {
    let (method, params) = match msg {
        JsonRpcMessage::Request(request) => (request.method.as_str(), request.params.as_ref()),
        JsonRpcMessage::Notification(note) => (note.method.as_str(), note.params.as_ref()),
        _ => ("", None),
    };
    let params = params.cloned().unwrap_or(Value::Null);
    let created = match method {
        "createSession" | "createTerminal" => wire::channel_of(&params),
        "createChat" => params
            .get("chat")
            .and_then(Value::as_str)
            .or_else(|| wire::channel_of(&params)),
        _ => None,
    };
    created
        .or_else(|| wire::channel_of(&params))
        .unwrap_or(root::URI)
        .to_string()
}

/// The timeout frame for a request that outlived its deadline (queued or
/// running).
fn timeout_frame(id: u64, method: &str, why: &str) -> JsonRpcMessage {
    wire::error(
        id,
        JsonRpcError {
            code: codes::X_MANOX_TIMEOUT,
            message: format!("{method} {why} within {}s", DISPATCH_DEADLINE.as_secs()),
            data: None,
        },
    )
}

/// Run one lane message in its order slot.
async fn run_one(inner: &Arc<Inner>, conn: &Arc<Conn>, lane: &str, queued: Queued) {
    // A lane drains its buffer after teardown drops the registry; the
    // connection is already dead by then, so the buffered work must not run.
    if !conn.alive() {
        return;
    }
    let Queued { msg, at } = queued;
    let deadline = at + DISPATCH_DEADLINE;
    match msg {
        JsonRpcMessage::Request(request) => {
            let id = request.id;
            let method = request.method.clone();
            // The claim was taken at dispatch; a request that sat past its
            // deadline in the queue is answered here, without running.
            if tokio::time::Instant::now() >= deadline {
                let answered = conn.reply_once(
                    id,
                    timeout_frame(id, &method, "expired in the lane queue before starting"),
                );
                tracing::warn!(
                    connection = conn.id(),
                    lane = lane,
                    method = %method,
                    answered = answered,
                    "request expired in the lane queue"
                );
                return;
            }
            let handler_inner = inner.clone();
            let handler_conn = conn.clone();
            let mut handler = tokio::spawn(async move {
                crate::router::handle(
                    &handler_inner,
                    &handler_conn,
                    JsonRpcMessage::Request(request),
                )
                .await;
            });
            tokio::select! {
                joined = &mut handler => {
                    finish(conn, lane, &method, Some(id), joined);
                }
                _ = tokio::time::sleep_until(deadline) => {
                    let answered = conn.reply_once(
                        id,
                        timeout_frame(id, &method, "did not answer"),
                    );
                    tracing::warn!(
                        connection = conn.id(),
                        lane = lane,
                        method = %method,
                        answered = answered,
                        "handler exceeded the dispatch deadline"
                    );
                    // Keep the lane's order: the handler holds its slot even
                    // though its reply is already refused.
                    let joined = handler.await;
                    finish(conn, lane, &method, Some(id), joined);
                }
            }
        }
        JsonRpcMessage::Notification(note) => {
            let method = note.method.clone();
            let handler_inner = inner.clone();
            let handler_conn = conn.clone();
            let mut handler = tokio::spawn(async move {
                crate::router::handle(
                    &handler_inner,
                    &handler_conn,
                    JsonRpcMessage::Notification(note),
                )
                .await;
            });
            tokio::select! {
                joined = &mut handler => {
                    finish(conn, lane, &method, None, joined);
                }
                _ = tokio::time::sleep_until(deadline) => {
                    tracing::warn!(
                        connection = conn.id(),
                        lane = lane,
                        method = %method,
                        "notification handler exceeded the dispatch deadline; its lane stays ordered"
                    );
                    // Order is the contract for notifications too: the next
                    // message on this channel waits for this one.
                    let joined = handler.await;
                    finish(conn, lane, &method, None, joined);
                }
            }
        }
        // The reader completes host → client waiters inline; one that reaches a
        // lane still resolves rather than being dropped.
        response => crate::router::handle(inner, conn, response).await,
    }
}

/// Claim the one-answer slot for an inbound request (see [`Conn::expect_reply`]).
///
/// A duplicate id is a client violation: the later copy is dropped, so one id
/// never receives two answers.
pub(crate) fn claim(conn: &Conn, id: u64) -> bool {
    if conn.expect_reply(id) {
        return true;
    }
    tracing::warn!(
        connection = conn.id(),
        id = id,
        "duplicate outstanding request id; the later copy is dropped"
    );
    false
}

/// Report how a spawned dispatch task ended.
///
/// A panicking handler would otherwise leave its request unanswered forever, so
/// it is answered [`codes::rpc::INTERNAL_ERROR`] unless the deadline already
/// answered it.
pub(crate) fn finish(
    conn: &Arc<Conn>,
    lane: &str,
    method: &str,
    request: Option<u64>,
    joined: Result<(), tokio::task::JoinError>,
) {
    let Err(err) = joined else {
        return;
    };
    tracing::error!(
        connection = conn.id(),
        lane = lane,
        method = %method,
        "dispatch task failed: {err}"
    );
    if let Some(id) = request {
        let answered = conn.reply_once(
            id,
            wire::error(
                id,
                JsonRpcError {
                    code: codes::rpc::INTERNAL_ERROR,
                    message: format!("{method} failed internally"),
                    data: None,
                },
            ),
        );
        tracing::warn!(
            connection = conn.id(),
            lane = lane,
            method = %method,
            answered = answered,
            "a panicked handler was answered with an internal error"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ahp_types::messages::{JsonRpcNotification, JsonRpcRequest, JsonRpcVersion};

    fn request(method: &str, params: Value) -> JsonRpcMessage {
        JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: JsonRpcVersion::V2,
            id: 7,
            method: method.to_string(),
            params: Some(params),
        })
    }

    fn notification(method: &str, params: Value) -> JsonRpcMessage {
        JsonRpcMessage::Notification(JsonRpcNotification {
            jsonrpc: JsonRpcVersion::V2,
            method: method.to_string(),
            params: Some(params),
        })
    }

    /// A message lanes on the channel it targets, so per-channel order is lane
    /// order.
    #[test]
    fn a_message_lanes_on_its_channel() {
        assert_eq!(
            target_channel(&request(
                "subscribe",
                serde_json::json!({"channel": "ahp-chat:/c-1"})
            )),
            "ahp-chat:/c-1"
        );
        assert_eq!(
            target_channel(&notification(
                "dispatchAction",
                serde_json::json!({"channel": "ahp-session:/s-1"})
            )),
            "ahp-session:/s-1"
        );
    }

    /// Creation lanes on the URI it creates, never on the channel that merely
    /// owns it: `createChat` on a busy session must not wait for that session.
    #[test]
    fn creation_lanes_on_the_uri_it_creates() {
        assert_eq!(
            target_channel(&request(
                "createChat",
                serde_json::json!({
                    "channel": "ahp-session:/s-1",
                    "chat": "ahp-chat:/c-2",
                })
            )),
            "ahp-chat:/c-2"
        );
        // A session/terminal creation's channel *is* the URI it creates.
        assert_eq!(
            target_channel(&request(
                "createSession",
                serde_json::json!({"channel": "ahp-session:/s-2"})
            )),
            "ahp-session:/s-2"
        );
        assert_eq!(
            target_channel(&request(
                "createTerminal",
                serde_json::json!({"channel": "ahp-terminal:/t-1"})
            )),
            "ahp-terminal:/t-1"
        );
    }

    /// Connection-level messages without a channel share the root lane.
    #[test]
    fn a_channel_less_message_lanes_on_root() {
        assert_eq!(target_channel(&request("ping", Value::Null)), root::URI);
        assert_eq!(
            target_channel(&request("listSessions", serde_json::json!({}))),
            root::URI
        );
    }
}
