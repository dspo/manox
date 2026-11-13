//! Lane conformance: the connection reader is a transport demultiplexer.
//!
//! One lane per target channel, so order is preserved where AHP promises it
//! (within a channel) and no channel can starve another. These suites speak raw
//! JSON-RPC over the in-process transport: the SDK client would serialize
//! requests and hide exactly the ordering and exactly-one-reply facts under
//! test.

mod common;

use std::sync::Arc;
use std::time::Duration;

use ahp::{Transport, TransportMessage};
use ahp_types::common::ROOT_RESOURCE_URI;
use ahp_types::messages::JsonRpcMessage;
use ahp_types::version::PROTOCOL_VERSION;
use manox_ahp::backend::Backend;
use manox_ahp::codes;
use manox_ahp::connection::Dialect;
use manox_ahp::lanes::LANE_CAPACITY;
use manox_ahp::transport::inproc::{self, InprocClientTransport};
use manox_ahp::{Host, ext, wire};

use common::{ParkingBackend, RecordingBackend, TestBackend};

/// A raw AHP client end: frames in, frames out, no SDK state in the way.
struct RawClient {
    transport: InprocClientTransport,
}

impl RawClient {
    fn new(transport: InprocClientTransport) -> Self {
        Self { transport }
    }

    async fn send(&mut self, msg: JsonRpcMessage) {
        self.transport
            .send(TransportMessage::Parsed(msg))
            .await
            .expect("frame sent");
    }

    async fn recv(&mut self) -> JsonRpcMessage {
        match self.transport.recv().await.expect("transport open") {
            Some(TransportMessage::Parsed(msg)) => msg,
            other => panic!("expected a parsed frame, got {other:?}"),
        }
    }
}

/// The handshake every suite needs: identity, root channel, no subscriptions.
fn initialize(client_id: &str) -> JsonRpcMessage {
    wire::request(
        1,
        "initialize",
        serde_json::json!({
            "channel": ROOT_RESOURCE_URI,
            "clientId": client_id,
            "protocolVersions": [PROTOCOL_VERSION],
        }),
    )
}

fn subscribe(id: u64, uri: &str) -> JsonRpcMessage {
    wire::request(id, "subscribe", serde_json::json!({ "channel": uri }))
}

/// Assert a frame is the success reply for `id`.
fn assert_success(frame: JsonRpcMessage, id: u64) {
    match frame {
        JsonRpcMessage::SuccessResponse(response) => assert_eq!(response.id, id),
        other => panic!("expected the success reply for {id}, got {other:?}"),
    }
}

/// A handler that never returns must not stop the connection.
///
/// This is the regression the whole PR exists for: with one reader awaiting every
/// handler inline, a single stuck `subscribe` blocked `createSession`, `ping` and
/// every other channel behind it.
#[tokio::test(flavor = "multi_thread")]
async fn a_parked_lane_does_not_stop_the_connection() {
    let backend = ParkingBackend::new();
    let host = Host::new(backend.clone() as Arc<dyn Backend>);
    let gate = backend.park("s-2");
    let (host_side, client_side) = inproc::pair();
    host.accept(host_side);
    let mut client = RawClient::new(client_side);

    client.send(initialize("lanes")).await;
    assert_success(client.recv().await, 1);

    // A request on session s-2 parks its lane forever.
    client.send(subscribe(2, "ahp-session:/s-2")).await;
    gate.wait_started().await;

    // `createSession` lanes on the session URI it creates, so it answers while
    // s-2's lane is still parked.
    client
        .send(wire::request(
            3,
            "createSession",
            serde_json::json!({ "channel": "ahp-session:/s-9" }),
        ))
        .await;
    let frame = tokio::time::timeout(Duration::from_secs(1), client.recv())
        .await
        .expect("createSession answers while another lane is parked");
    assert_success(frame, 3);

    // So does a request on a different existing channel.
    client.send(subscribe(4, "ahp-session:/s-1")).await;
    let frame = tokio::time::timeout(Duration::from_secs(1), client.recv())
        .await
        .expect("another channel answers while s-2 is parked");
    assert_success(frame, 4);
}

/// Order within a channel is the lane's contract: a `subscribe` and the
/// `dispatchAction` that follows it cannot be reordered.
#[tokio::test(flavor = "multi_thread")]
async fn one_channel_keeps_subscribe_then_dispatch_in_order() {
    let backend = RecordingBackend::new();
    let host = Host::new(backend.clone() as Arc<dyn Backend>);
    let (host_side, client_side) = inproc::pair();
    host.accept(host_side);
    let mut client = RawClient::new(client_side);

    client.send(initialize("ordered")).await;
    assert_success(client.recv().await, 1);

    // Both frames go out before either is answered: the ordering under test is
    // the host's, not the client's patience.
    client.send(subscribe(2, &TestBackend::chat_uri())).await;
    client
        .send(wire::notification(
            "dispatchAction",
            serde_json::json!({
                "channel": TestBackend::chat_uri(),
                "clientSeq": 1,
                "action": {
                    "type": "chat/pendingMessageRemoved",
                    "kind": "steering",
                    "id": "p-1",
                },
            }),
        ))
        .await;

    // The subscribe's reply leads, then the echo of the dispatch.
    assert_success(client.recv().await, 2);
    let echo = client.recv().await;
    let JsonRpcMessage::Notification(note) = echo else {
        panic!("expected the dispatch echo, got {echo:?}");
    };
    assert_eq!(note.method, wire::ACTION_METHOD);
    let envelope = note.params.expect("the echo carries its envelope");
    assert_eq!(envelope["action"]["type"], "chat/pendingMessageRemoved");

    // The backend's own order proves the seeding work finished before the
    // dispatch started (a dispatch outrunning its subscribe is refused).
    let events = backend.events();
    let seeded = events
        .iter()
        .position(|event| event == "chatState:c-1")
        .unwrap_or_else(|| panic!("the subscribe never seeded the chat: {events:?}"));
    let dispatched = events
        .iter()
        .position(|event| event.starts_with("dispatch:"))
        .unwrap_or_else(|| panic!("the dispatch never reached the backend: {events:?}"));
    assert!(
        seeded < dispatched,
        "the dispatch started before its subscribe finished: {events:?}"
    );
    assert_eq!(
        backend.dispatched().len(),
        1,
        "the dispatch was accepted exactly once"
    );
}

/// A request is answered exactly once: the dispatch deadline answers it, and the
/// handler's eventual reply must not become a second frame.
///
/// Time is paused, so the 30-second deadline costs the suite nothing: the runtime
/// advances to the timer when the connection has nothing else to do.
#[tokio::test(start_paused = true)]
async fn a_deadline_answers_once_and_discards_the_late_reply() {
    let backend = ParkingBackend::new();
    let host = Host::new(backend.clone() as Arc<dyn Backend>);
    let gate = backend.park("s-2");
    let (host_side, client_side) = inproc::pair();
    host.accept(host_side);
    let mut client = RawClient::new(client_side);

    client.send(subscribe(7, "ahp-session:/s-2")).await;
    gate.wait_started().await;

    // The handler is stuck; the lane's deadline answers for it.
    let frame = client.recv().await;
    let JsonRpcMessage::ErrorResponse(response) = frame else {
        panic!("expected the deadline's error reply, got {frame:?}");
    };
    assert_eq!(response.id, 7);
    assert_eq!(response.error.code, codes::X_MANOX_TIMEOUT);

    // Release the handler and let it run to its (discarded) reply.
    gate.release_one();
    gate.wait_completed().await;
    tokio::task::yield_now().await;

    // The next frame answers a later request, proving the late reply never
    // travelled: it was a second answer for id 7 and the claim was gone.
    client
        .send(wire::request(8, "ping", serde_json::Value::Null))
        .await;
    let frame = client.recv().await;
    assert_success(frame, 8);
}

/// A request that spends its whole deadline queued behind another channel
/// handler is answered without being executed: the deadline covers queue
/// time, not only handler time.
#[tokio::test(start_paused = true)]
async fn a_request_that_expires_in_the_queue_is_answered_without_running() {
    let backend = ParkingBackend::new();
    let host = Host::new(backend.clone() as Arc<dyn Backend>);
    let gate = backend.park("s-2");
    let (host_side, client_side) = inproc::pair();
    host.accept(host_side);
    let mut client = RawClient::new(client_side);

    // The first request parks the lane's handler.
    client.send(subscribe(11, "ahp-session:/s-2")).await;
    gate.wait_started().await;
    // The second queues behind it.
    client.send(subscribe(12, "ahp-session:/s-2")).await;
    tokio::task::yield_now().await;

    // Past the deadline, the parked handler is answered by the timeout.
    tokio::time::advance(Duration::from_secs(31)).await;
    let frame = client.recv().await;
    let JsonRpcMessage::ErrorResponse(response) = frame else {
        panic!("expected the deadline's error reply, got {frame:?}");
    };
    assert_eq!(response.id, 11);
    assert_eq!(response.error.code, codes::X_MANOX_TIMEOUT);

    gate.release_one();
    gate.wait_completed().await;

    // The queued request is answered too, without running: if it had run it
    // would have parked on the same gate and sent no frame.
    let frame = client.recv().await;
    let JsonRpcMessage::ErrorResponse(response) = frame else {
        panic!("expected the queued request's timeout, got {frame:?}");
    };
    assert_eq!(response.id, 12);
    assert_eq!(response.error.code, codes::X_MANOX_TIMEOUT);
}

/// A handshake whose cold seed never returns is answered with a timeout and
/// the connection is retired: it must not stall the reader for every lane.
#[tokio::test(start_paused = true)]
async fn a_handshake_that_cannot_complete_is_retired() {
    let backend = ParkingBackend::new();
    let host = Host::new(backend.clone() as Arc<dyn Backend>);
    let gate = backend.park("s-2");
    let (host_side, client_side) = inproc::pair();
    host.accept(host_side);
    let mut client = RawClient::new(client_side);

    client
        .send(wire::request(
            1,
            "initialize",
            serde_json::json!({
                "channel": ROOT_RESOURCE_URI,
                "clientId": "wedge",
                "protocolVersions": [PROTOCOL_VERSION],
                "initialSubscriptions": ["ahp-session:/s-2"],
            }),
        ))
        .await;
    // The handshake's cold seed parks; its deadline answers and retires the
    // connection.
    let frame = client.recv().await;
    let JsonRpcMessage::ErrorResponse(response) = frame else {
        panic!("expected the handshake timeout, got {frame:?}");
    };
    assert_eq!(response.id, 1);
    assert_eq!(response.error.code, codes::X_MANOX_TIMEOUT);

    // Let the detached handler finish so the task does not outlive the suite.
    gate.release_one();
    gate.wait_completed().await;
}

/// A full lane answers overload instead of blocking the reader, and other
/// channels keep being served while it stays full.
#[tokio::test(flavor = "multi_thread")]
async fn a_full_lane_answers_overload_and_other_channels_still_serve() {
    let backend = ParkingBackend::new();
    let host = Host::new(backend.clone() as Arc<dyn Backend>);
    let gate = backend.park("s-2");
    let (host_side, client_side) = inproc::pair();
    host.accept(host_side);
    let mut client = RawClient::new(client_side);

    client.send(initialize("overload")).await;
    assert_success(client.recv().await, 1);

    // The first message occupies the lane (and never returns) ...
    client.send(subscribe(2, "ahp-session:/s-2")).await;
    gate.wait_started().await;
    // ... the next LANE_CAPACITY messages fill its buffer ...
    for id in 3..(3 + LANE_CAPACITY as u64) {
        client.send(subscribe(id, "ahp-session:/s-2")).await;
    }
    // ... and this one cannot be queued at all.
    let overflow = 3 + LANE_CAPACITY as u64;
    client.send(subscribe(overflow, "ahp-session:/s-2")).await;

    let frame = tokio::time::timeout(Duration::from_secs(1), client.recv())
        .await
        .expect("the overflow is answered, not queued");
    let JsonRpcMessage::ErrorResponse(response) = frame else {
        panic!("expected the overload reply, got {frame:?}");
    };
    assert_eq!(response.id, overflow);
    assert_eq!(response.error.code, codes::X_MANOX_OVERLOAD);

    // Another channel is still served while the lane stays full.
    client
        .send(wire::request(
            9_000,
            "createSession",
            serde_json::json!({ "channel": "ahp-session:/s-9" }),
        ))
        .await;
    let frame = tokio::time::timeout(Duration::from_secs(1), client.recv())
        .await
        .expect("another channel answers while the lane is full");
    assert_success(frame, 9_000);
}

/// Panics the reader on a `test/poison` notification — the only way to prove
/// teardown is a guard rather than a happy-path tail.
struct PoisonDialect;

impl Dialect for PoisonDialect {
    fn incoming(&self, msg: JsonRpcMessage) -> JsonRpcMessage {
        if let JsonRpcMessage::Notification(note) = &msg
            && note.method == "test/poison"
        {
            panic!("poisoned reader");
        }
        msg
    }
}

/// A panicking reader still tears the connection down: its waiters are cancelled
/// and it leaves the host's registry.
///
/// Before the guard, a panic left the connection "alive" with waiters that never
/// resolve, so every later host → client request to it waited out its whole
/// deadline.
#[tokio::test(flavor = "multi_thread")]
async fn a_panicking_reader_cancels_waiters_and_removes_the_connection() {
    let host = Host::new(TestBackend::new() as Arc<dyn Backend>);
    let (host_side, client_side) = inproc::pair();
    host.accept_with_dialect(host_side, Box::new(PoisonDialect));
    let mut client = RawClient::new(client_side);

    // Declare a host → client capability and subscribe to the seeded session, so
    // `request_client` selects this connection.
    client
        .send(wire::request(
            1,
            "initialize",
            serde_json::json!({
                "channel": ROOT_RESOURCE_URI,
                "clientId": "poison",
                "protocolVersions": [PROTOCOL_VERSION],
                "initialSubscriptions": [TestBackend::session_uri()],
                "_meta": { ext::META_KEY: { "serverRequests": [ext::requests::CLIPBOARD_READ] } },
            }),
        ))
        .await;
    assert_success(client.recv().await, 1);

    // Park a host → client request on this connection.
    let pending = tokio::spawn({
        let host = host.clone();
        async move {
            // `request_client` takes a session *id* (it derives the URI).
            host.request_client("s-1", ext::requests::CLIPBOARD_READ, serde_json::json!({}))
                .await
        }
    });
    // Reading the frame proves the waiter is registered before the panic.
    let request = client.recv().await;
    let JsonRpcMessage::Request(request) = request else {
        panic!("expected the host → client request, got {request:?}");
    };
    assert_eq!(request.method, ext::requests::CLIPBOARD_READ);

    // Poison the reader.
    client
        .send(wire::notification("test/poison", serde_json::Value::Null))
        .await;

    let waiter = tokio::time::timeout(Duration::from_secs(2), pending)
        .await
        .expect("the waiter is resolved, not left open")
        .expect("the request task joins");
    let err = waiter.expect_err("the cancelled waiter fails closed");
    assert!(
        err.to_string().contains("closed"),
        "the waiter names the closed connection: {err}"
    );

    tokio::time::timeout(Duration::from_secs(2), async {
        while host.connection_count() != 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the connection leaves the host's registry");
}
