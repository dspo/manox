//! napi-rs bindings exposing the manox agent core to a TypeScript host.
//!
//! Thin glue over `AgentServer` + `InProcessConnection`: the host starts the
//! agent runtime, opens an in-process protocol connection, and streams typed
//! `FromServer` messages (serialized as JSON strings) back to Node through a
//! threadsafe function. `shutdown()` disconnects the transport so window
//! reloads and `deactivate` re-initialize cleanly.
//!
//! T9 (v2 frames): both directions are envelope-generic — the pump forwards
//! every [`FromServer`] variant (including the v2 `StreamItem` / `StreamEnd`
//! / `Host` arms) and [`send_command`] accepts every [`FromClient`] variant
//! (including `StreamOpen` / `StreamCancel`). No frame vocabulary is
//! hardcoded here, so new protocol frames flow through without changes.

#[macro_use]
extern crate napi_derive;

use std::sync::Mutex;

use napi::bindgen_prelude::*;
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};

use manox_protocol::AnswerKind;
use manox_protocol::msg::FromClient;
use manox_protocol::transport::{InProcessConnection, RpcConnection};
use manox_session_core::agent_client::AgentClient;
use manox_session_core::agent_server::AgentServer;

/// The "client" end of the in-process connection. The AgentServer holds the
/// server end; the napi side sends commands to the server and receives events
/// from the server through this connection.
static CONN: Mutex<Option<ConnectionState>> = Mutex::new(None);

struct ConnectionState {
    conn: InProcessConnection,
    /// The pump thread handle. Dropped on shutdown.
    _pump: std::thread::JoinHandle<()>,
    /// The server handle. Dropped on shutdown (the server's tasks settle when
    /// the connection is disconnected).
    _server: AgentServer,
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

/// Start the agent runtime and open an in-process protocol connection to the
/// AgentServer. `event_cb` receives one serialized `FromServer` JSON string
/// per call; it runs on the Node main thread, scheduled by the pump thread
/// through a threadsafe function.
///
/// `client_id` is the host's stable identity (§D.2 Initialize): the VS Code
/// extension persists it (globalState) so reconnects re-seat as the same
/// client rather than minting a new one. An empty string falls back to the
/// legacy `"vscode"` pin.
///
/// The handshake (`FromClient::Request(Initialize{...})`) is sent automatically
/// so the TS side never needs to send it.
#[napi]
pub fn start(client_id: String, event_cb: JsFunction) -> Result<()> {
    // The built-in Chrome engine (rustwright-core, via ChromeUse) is NOT
    // linked into the VS Code host: the agent is built without the
    // `chrome-use` feature on the manox-napi edge, so nothing here can ever
    // launch it. DISABLE_TELEMETRY is still set as defense in depth in case
    // the engine is ever enabled on this host.
    unsafe { std::env::set_var("DISABLE_TELEMETRY", "1") };
    let mut slot = conn_slot();
    if slot.is_some() {
        return Err(napi::Error::from_reason("actor already started"));
    }

    // Pin the host identity and initialize the agent runtime.
    manox_agent::host::set_host(manox_agent::host::Host::Vscode);
    manox_agent::init();

    let tsfn: ThreadsafeFunction<String> =
        event_cb.create_threadsafe_function(0, |ctx| Ok(vec![ctx.value]))?;

    let cwd = std::env::current_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("/"))
        .to_string_lossy()
        .to_string();

    // Create the AgentServer, then connect through the unified `AgentClient`
    // wrapper (in-process pair + `Initialize` handshake). The caller-supplied
    // `client_id` (persisted by the extension) pins the identity so a window
    // reload / re-initialize re-seats the server-side owner instead of
    // registering a fresh client.
    let server = AgentServer::new(std::path::PathBuf::from(&cwd));
    let client_id = if client_id.trim().is_empty() {
        "vscode".to_string()
    } else {
        client_id
    };
    let client = AgentClient::connect(
        &server,
        client_id,
        // VS Code has approval UI (sidebar), AskUserQuestion (answer_question
        // in sidebar), the OS clipboard
        // (text-only bridging), the system external-link opener, and hosts
        // client-contributed tools (registerSessionTools/invokeClientTool).
        // No browser capability (no embedded browser surface).
        vec![
            AnswerKind::Approve,
            AnswerKind::AskUserQuestion,
            AnswerKind::ClipboardRead,
            AnswerKind::OpenExternal,
            AnswerKind::ClientTool,
        ],
        vec![],
    );
    let client_conn = client.into_conn();

    // Spawn the pump thread: reads `FromServer` messages from the client end
    // of the connection and pushes them as JSON strings to the Node callback.
    // The channel is typed `FromServer`, so every variant — including the v2
    // `StreamItem` / `StreamEnd` / `Host` arms — is forwarded verbatim; the
    // pump never filters or interprets frames.
    let pump_conn = client_conn.clone();
    let pump = std::thread::Builder::new()
        .name("manox-napi-pump".into())
        .spawn(move || {
            let rx = pump_conn.server_rx();
            while let Ok(msg) = rx.recv_blocking() {
                match serde_json::to_string(&msg) {
                    Ok(json) => {
                        let _ = tsfn.call(Ok(json), ThreadsafeFunctionCallMode::NonBlocking);
                    }
                    Err(e) => {
                        eprintln!("manox-napi: failed to serialize FromServer: {e}");
                    }
                }
            }
        })
        .map_err(|e| napi::Error::from_reason(format!("failed to spawn pump thread: {e}")))?;

    *slot = Some(ConnectionState {
        conn: client_conn,
        _pump: pump,
        _server: server,
    });
    Ok(())
}

/// Deliver a serialized `FromClient` JSON string to the AgentServer.
///
/// The payload is parsed against the full [`FromClient`] envelope — every
/// variant is accepted, including the v2 `StreamOpen` / `StreamCancel`
/// frames. The envelope is a *closed* enum: an unknown `kind` tag fails the
/// parse with an error surfaced to the JS caller (never a panic); the TS
/// relay layer logs + drops such frames, so a future webview frame this
/// host's protocol crate cannot decode degrades gracefully.
#[napi]
pub fn send_command(command: String) -> Result<()> {
    let msg: FromClient = serde_json::from_str(&command).map_err(|e| {
        napi::Error::from_reason(format!(
            "invalid FromClient JSON: {e}. Expected: \
             {{\"kind\":\"request\"|\"notification\"|\"reply\"|\"streamOpen\"|\"streamCancel\", ...}} \
             — see FromClient type in protocol.d.ts"
        ))
    })?;
    match conn_slot().as_ref() {
        Some(state) => {
            state.conn.send_to_server(msg);
            Ok(())
        }
        None => Err(napi::Error::from_reason(
            "actor not started; call start(callback) first",
        )),
    }
}

/// Tear down the agent connection and release the event callback. Safe to
/// call when the actor is not started; intended for host `deactivate`.
#[napi]
pub fn shutdown() {
    if let Some(state) = conn_slot().take() {
        state.conn.disconnect();
        // The pump thread exits when the channel is closed (disconnect closes
        // both directions). The AgentServer's tasks settle on the tokio runtime
        // when the connection drops.
    }
}

#[cfg(test)]
mod tests {
    //! T9 contract tests: the v2 frame classes flow through the napi glue
    //! paths unmodified.
    //!
    //! * The pump path is `InProcessConnection::server_rx() -> serde_json::
    //!   to_string -> tsfn`: typed `FromServer`, so any variant (including
    //!   `StreamItem` / `StreamEnd` / `Host`) is forwarded verbatim. These
    //!   tests drive a real in-process pair, serialize what the pump would
    //!   serialize, and assert the exact JSON key shapes the wire surface
    //!   declares (`wire_surface!` in `manox-protocol/src/surface.rs`; the
    //!   former TS binding mirror was retired with the frontends, round 3
    //!   §二.9).
    //! * The `send_command` path is `serde_json::from_str::<FromClient>`:
    //!   `StreamOpen` / `StreamCancel` must parse, and an unknown `kind` tag
    //!   must surface an error (never a panic) — the TS relay logs and drops
    //!   such frames.

    use std::collections::BTreeMap;

    use manox_protocol::handshake::Initialize;
    use manox_protocol::journal::JournalWireEvent;
    use manox_protocol::stream::{
        HostEvent, SessionSnapshot, StreamEndReason, StreamFrame, StreamKind,
    };
    use manox_protocol::transport::{RpcConnection, in_process_pair};
    use manox_protocol::{
        ClientCall, ClientNote, FromClient, FromServer, MsgId, ServerCall, ServerNote, StreamId,
        ThreadHeader,
    };
    use serde_json::{Value, json};

    fn header() -> ThreadHeader {
        ThreadHeader {
            id: "s1".into(),
            cwd: "/tmp".into(),
            parent_session: None,
            metadata: None,
            created_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    /// Serialize exactly as the pump does and parse back the way the webview
    /// (JSON.parse of the postMessage payload) would.
    fn pump_wire(msg: &FromServer) -> Value {
        let json = serde_json::to_string(msg).expect("pump serialization");
        serde_json::from_str(&json).expect("pump output is valid JSON")
    }

    /// Every `FromServer` v2 arm survives the pump path (typed channel ->
    /// serde -> Node callback) with the wire tags the TS bindings declare.
    #[test]
    fn pump_forwards_all_v2_from_server_frames() {
        let (client, server) = in_process_pair();

        let frames = vec![
            FromServer::StreamItem {
                stream_id: StreamId::new("webui-stream-1"),
                frame: StreamFrame::Snapshot(SessionSnapshot {
                    session_id: "s1".into(),
                    header: header(),
                    cursor: 7,
                    records: vec![],
                    has_more: false,
                    projections: BTreeMap::from([("title".into(), json!("t"))]),
                    projections_as_of_seq: 7,
                }),
            },
            FromServer::StreamItem {
                stream_id: StreamId::new("webui-stream-1"),
                frame: StreamFrame::Entry {
                    seq: 8,
                    id: "e-8".into(),
                    parent_id: Some("e-7".into()),
                    timestamp: "2026-09-05T00:00:00Z".into(),
                    event: JournalWireEvent::AgentTextDelta { s: "hi".into() },
                },
            },
            FromServer::StreamEnd {
                stream_id: StreamId::new("webui-stream-1"),
                reason: StreamEndReason::Resync,
            },
            FromServer::Host {
                host: HostEvent::SessionStatus {
                    session_id: "s1".into(),
                    running: Some(true),
                    errored: Some(false),
                    unread: None,
                    pending_auth: None,
                    pending_plan: None,
                    background_work: None,
                },
            },
            FromServer::Response {
                id: MsgId::new("webui-1"),
                outcome: Ok(json!({ "accepted": true })),
            },
            FromServer::Request {
                id: MsgId::new("auth-1"),
                call: ServerCall::Approve {
                    // GW3: the gateway mints delivery ids; this scripted
                    // frame carries a stable one.
                    delivery_id: "dlv-s1-1".into(),
                    session_id: "s1".into(),
                    auth_id: "auth-1".into(),
                    tool_name: "bash".into(),
                    summary: "run".into(),
                    input: json!({ "command": "ls" }),
                },
            },
            // The legacy notification arm still flows through unchanged
            // (dual-protocol window, §K.5).
            FromServer::Notification {
                note: ServerNote::Ready,
            },
        ];

        for frame in frames.clone() {
            server.send_to_client(frame);
        }
        for frame in frames {
            let on_wire = client
                .server_rx()
                .recv_blocking()
                .expect("typed channel carries the variant");
            assert_eq!(on_wire, frame);
        }
    }

    /// Exact JSON key shapes the TS-side guards (`protocol.d.ts` +
    /// `parseFromServer`) match on.
    #[test]
    fn v2_frame_wire_shapes_match_ts_bindings() {
        let item = pump_wire(&FromServer::StreamItem {
            stream_id: StreamId::new("st-1"),
            frame: StreamFrame::Entry {
                seq: 3,
                id: "e-3".into(),
                parent_id: None,
                timestamp: "2026-09-05T00:00:00Z".into(),
                event: JournalWireEvent::AgentTextDelta { s: "x".into() },
            },
        });
        assert_eq!(item["kind"], "streamItem");
        assert_eq!(item["streamId"], "st-1");
        assert_eq!(item["frame"]["type"], "entry");
        // U5 envelope keys (the §C.1 full frame) — the guard never covered
        // them when the envelope landed; the TS parseFromServer reads them.
        assert_eq!(item["frame"]["seq"], 3);
        assert_eq!(item["frame"]["id"], "e-3");
        assert_eq!(item["frame"]["parentId"], serde_json::Value::Null);
        assert_eq!(item["frame"]["timestamp"], "2026-09-05T00:00:00Z");
        assert_eq!(item["frame"]["event"]["type"], "agentTextDelta");

        let end = pump_wire(&FromServer::StreamEnd {
            stream_id: StreamId::new("st-1"),
            reason: StreamEndReason::Failure {
                code: "resync-required".into(),
                message: "overflow".into(),
            },
        });
        assert_eq!(end["kind"], "streamEnd");
        assert_eq!(end["reason"]["type"], "failure");
        assert_eq!(end["reason"]["code"], "resync-required");

        let host = pump_wire(&FromServer::Host {
            host: HostEvent::Ready { epoch: 2 },
        });
        assert_eq!(host["kind"], "host");
        assert_eq!(host["host"]["type"], "ready");
        assert_eq!(host["host"]["epoch"], 2);
    }

    /// `send_command` parses every `FromClient` v2 arm, including the stream
    /// frames the webview now mints.
    #[test]
    fn send_command_accepts_v2_stream_frames() {
        let cases: Vec<FromClient> = vec![
            FromClient::StreamOpen {
                stream_id: StreamId::new("webui-stream-1"),
                stream_kind: StreamKind::FollowSession {
                    session_id: "s1".into(),
                    max_messages: None,
                },
            },
            FromClient::StreamCancel {
                stream_id: StreamId::new("webui-stream-1"),
            },
            FromClient::Request {
                id: MsgId::new("webui-2"),
                call: ClientCall::Initialize(Initialize {
                    client_id: "vscode-9f2c".into(),
                    capabilities: vec![],
                    sessions: vec![],
                    // C1: this client speaks the current protocol epoch.
                    protocol_epoch: manox_protocol::PROTOCOL_EPOCH,
                }),
            },
            FromClient::Notification {
                note: ClientNote::CancelTurn {
                    session_id: "s1".into(),
                },
            },
            FromClient::Reply {
                id: MsgId::new("auth-1"),
                outcome: Ok(json!({ "allow": true })),
            },
        ];
        for (i, case) in cases.iter().enumerate() {
            // The TS relay sends exactly the pump-side serialization:
            // `JSON.stringify` of a binding-shaped object. Round-trip it.
            let wire = serde_json::to_value(case).unwrap();
            if i == 0 {
                assert_eq!(wire["kind"], "streamOpen");
                assert_eq!(wire["streamId"], "webui-stream-1");
                assert_eq!(wire["streamKind"]["type"], "followSession");
            }
            let parsed: FromClient =
                serde_json::from_value(wire).unwrap_or_else(|e| panic!("{case:?}: {e}"));
            assert_eq!(&parsed, case);
        }
    }

    /// Closed-enum tolerance: an unknown `kind` tag fails the parse as an
    /// `Err` (surfaced to JS by `send_command`, logged + dropped by the TS
    /// relay) — never a panic. Pins the T9 status quo.
    #[test]
    fn unknown_from_client_kind_errors_without_panicking() {
        let err = serde_json::from_str::<FromClient>(r#"{"kind":"timeTravel","when":"now"}"#)
            .expect_err("closed enum rejects unknown tags");
        assert!(err.to_string().contains("timeTravel") || !err.to_string().is_empty());
    }

    /// #803 follow-up: the napi edge constructs its server through
    /// `AgentServer::new` (the `start` function above) and never touches
    /// `global()` — the shipped coverage hole was that the embedder-tool
    /// provider installed only inside `global`, so the VS Code host's
    /// `embedder_tools::provider()` stayed `None` and client-registered
    /// tools never reached the model.
    ///
    /// This test mirrors the host's actual construction (`AgentServer::new`)
    /// end-to-end through the unified `AgentClient` wrapper the binding
    /// uses, with NO manual `set_provider`: the slot the engine's tool
    /// assembly consults must be Some, and a tool registered via the real
    /// `RegisterSessionTools` request must surface as a `client_*` adapter
    /// through that installed provider. The session-core companion
    /// (`new_installs_the_embedder_provider_on_the_napi_edge`) covers the
    /// same edge from inside the library; this is the napi-crate-level pin
    /// proving the binding's own construction line wires the provider.
    ///
    /// Slot hygiene needs no `drop_provider_for_test` here (that helper is
    /// `test-support`-gated and deliberately absent from this crate's
    /// feature graph): `set_provider` is last-wins and this is the ONLY
    /// AgentServer construction in the napi test binary, so the slot read
    /// after the constructor is exactly this test's install — pre-fix it
    /// is `None` and the expect fails.
    ///
    /// Only compiled under the full runtime (the `mcp` feature — a member
    /// of `manox-napi`'s default set): the lean `--no-default-features`
    /// addon build (the VS Code-harness staging, `script/build-napi`) is a
    /// check-only lib leg that never activates `cfg(test)`, and the gate
    /// keeps the lean feature surface untouched (the a6df124e TEST_HOME
    /// lesson). The workspace test legs run it.
    #[cfg(feature = "mcp")]
    #[test]
    fn napi_start_equivalent_construction_installs_the_embedder_provider() {
        // `tools_for` resolves on the `Arc<dyn EmbedderToolProvider>` slot
        // value without a trait import; the register/handshake frames ride
        // the module-level `manox_protocol` imports.
        use manox_protocol::answer_kind::AnswerKind;
        use manox_session_core::agent_client::AgentClient;
        use manox_session_core::agent_server::AgentServer;

        // The host's boot: init the runtime and provider registry (the
        // `start` fn does the same through `manox_agent::init`), then
        // build the server EXACTLY like `start` does —
        // `AgentServer::new(cwd)`, never `global()`.
        manox_agent::runtime::init();
        manox_agent::provider_glue::init();
        let server = AgentServer::new(std::path::PathBuf::from("/"));
        let installed = manox_agent::embedder_tools::provider()
            .expect("AgentServer::new (the napi `start` construction) must install the provider");

        // Drive a real `RegisterSessionTools` through the same wrapper the
        // binding's `start` uses, then read back through the installed
        // provider — the engine's tool-assembly lookup point.
        let client =
            AgentClient::connect(&server, "napi-edge", vec![AnswerKind::ClientTool], vec![]);
        // Drain the handshake frames (Initialize ack + v1 Ready note +
        // the Host Ready epoch echo) so the register response is next.
        let recv = || {
            let rx = client.conn().server_rx();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                assert!(std::time::Instant::now() < deadline, "server_rx stalled");
                if let Ok(m) = rx.try_recv() {
                    return m;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        };
        for _ in 0..3 {
            recv();
        }
        let reg_id = client.send_call(ClientCall::RegisterSessionTools {
            session_id: "et-napi".into(),
            client_id: "napi-edge".into(),
            tools: vec![manox_protocol::client::ClientToolSpec {
                name: "get_selection".into(),
                description: "napi edge probe".into(),
                input_schema: serde_json::json!({"type": "object"}),
                read_only: true,
            }],
        });
        // Await the register response (tolerant drain: any other frame is
        // dropped, the id match settles the call).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            assert!(
                std::time::Instant::now() < deadline,
                "register never answered"
            );
            if let FromServer::Response { id, outcome } = recv()
                && id == reg_id
            {
                let registered = outcome.expect("register succeeds")["registered"].as_u64();
                assert_eq!(registered, Some(1));
                break;
            }
        }

        let tools = installed.tools_for("et-napi");
        let tool = tools
            .iter()
            .find(|t| t.name() == "client_get_selection")
            .expect("the napi-installed provider feeds the client_* adapter to the engine");
        assert_eq!(tool.description(), "napi edge probe");
    }
}
