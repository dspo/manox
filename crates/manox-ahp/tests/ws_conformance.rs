//! WebSocket conformance: the same scenario the in-process suite runs, over a
//! real socket with real JSON frames.
//!
//! The host side is the axum adapter mounted on `/ahp` — the same route the
//! gateway serves — and the client is the SDK's own `ahp-ws` transport. Both
//! suites compare against the *same* expected log
//! ([`common::EXPECTED_LOG`]), which is what makes "in-process typed ≡ WebSocket
//! serialized" an assertion rather than a hope.

#![cfg(feature = "axum-ws")]

mod common;

use std::sync::Arc;

use axum::Router;
use axum::extract::{State, ws::WebSocketUpgrade};
use axum::response::Response;
use axum::routing::any;
use manox_ahp::Host;
use manox_ahp::backend::Backend;

use common::TestBackend;

async fn ahp_route(State(host): State<Host>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| async move {
        host.accept(manox_ahp::transport::axum_ws::from_socket(socket));
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn websocket_scenario_matches_the_in_process_log() {
    let host = Host::new(TestBackend::new() as Arc<dyn Backend>);
    let app = Router::new()
        .route("/ahp", any(ahp_route))
        .with_state(host.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("binds loopback");
    let addr = listener.local_addr().expect("bound address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let transport = ahp_ws::WebSocketTransport::connect(&format!("ws://{addr}/ahp"))
        .await
        .expect("dials the host");

    let log = common::run_scenario(host.clone(), transport).await;
    assert_eq!(
        log,
        common::EXPECTED_LOG,
        "wire log diverged from in-process"
    );
}
