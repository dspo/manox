//! End to end over the in-process transport (the plan's `e2e_in_process` gate).
//!
//! The desktop path — typed messages, no socket — driven by the SDK's own
//! `ahp::Client`, so host and client really are the two ends of the protocol
//! rather than a self-consistent mock.

mod common;

use std::sync::Arc;
use std::time::Duration;

use ahp::{Client, ClientConfig};
use ahp_types::commands::ListSessionsParams;
use ahp_types::common::ROOT_RESOURCE_URI;
use ahp_types::state::AgentInfo;
use ahp_types::version::PROTOCOL_VERSION;
use manox_ahp::backend::Backend;
use manox_ahp::channels::root;
use manox_ahp::transport::inproc;
use manox_ahp::{Host, StateAction};

use common::{TestBackend, action};

fn connect(host: &Host) -> impl std::future::Future<Output = Client> {
    let (host_side, client_side) = inproc::pair();
    host.accept(host_side);
    async move {
        Client::connect(client_side, ClientConfig::default())
            .await
            .expect("connects")
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn initialize_returns_snapshots_and_the_extension_declaration() {
    let host = Host::new(TestBackend::new() as Arc<dyn Backend>);
    let client = connect(&host).await;

    let init = client
        .initialize(
            "desktop".to_string(),
            vec![PROTOCOL_VERSION.to_string()],
            vec![
                ROOT_RESOURCE_URI.to_string(),
                TestBackend::session_uri(),
                TestBackend::chat_uri(),
            ],
        )
        .await
        .expect("initializes");

    assert_eq!(init.protocol_version, PROTOCOL_VERSION);
    assert_eq!(
        init.snapshots.len(),
        3,
        "one snapshot per initial subscription"
    );
    assert_eq!(init.snapshots[0].resource, ROOT_RESOURCE_URI);
    let meta = init.meta.expect("_meta is advertised");
    let declaration = &meta["x-manox"];
    assert_eq!(declaration["version"], 1);
    assert!(
        declaration["channels"]
            .as_array()
            .is_some_and(|c| !c.is_empty())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn unsupported_protocol_version_is_refused_with_the_supported_list() {
    let host = Host::new(TestBackend::new() as Arc<dyn Backend>);
    let client = connect(&host).await;

    let err = client
        .initialize("desktop".to_string(), vec!["9.9.9".to_string()], vec![])
        .await
        .expect_err("must refuse");
    let message = format!("{err:?}");
    assert!(
        message.contains("32005") || message.to_lowercase().contains("version"),
        "unexpected error: {message}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn publish_reaches_subscribers_with_a_monotonic_server_seq() {
    let host = Host::new(TestBackend::new() as Arc<dyn Backend>);
    let client = connect(&host).await;
    client
        .initialize(
            "desktop".to_string(),
            vec![PROTOCOL_VERSION.to_string()],
            vec![],
        )
        .await
        .expect("initializes");
    let (result, mut sub) = client
        .subscribe(TestBackend::session_uri())
        .await
        .expect("subscribes");
    assert!(result.snapshot.is_some(), "sessions carry state");

    host.publish(
        &TestBackend::session_uri(),
        action(serde_json::json!({
            "type": "session/titleChanged",
            "title": "renamed by the runtime",
        })),
        None,
    );

    let event = tokio::time::timeout(Duration::from_secs(2), sub.recv())
        .await
        .expect("action arrives")
        .expect("subscription open");
    let envelope = match event {
        ahp::SubscriptionEvent::Action(envelope) => envelope,
        other => panic!("expected an action envelope, got {other:?}"),
    };
    assert_eq!(envelope.server_seq, 1);
    assert_eq!(envelope.channel, TestBackend::session_uri());
    assert!(envelope.origin.is_none());

    // The next stamp is strictly greater: AHP sequences the whole server.
    let second = host.publish(
        &TestBackend::session_uri(),
        action(serde_json::json!({"type": "session/titleChanged", "title": "again"})),
        None,
    );
    assert_eq!(second.server_seq, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn dispatch_action_is_echoed_back_with_origin() {
    let host = Host::new(TestBackend::new() as Arc<dyn Backend>);
    let client = connect(&host).await;
    client
        .initialize(
            "desktop".to_string(),
            vec![PROTOCOL_VERSION.to_string()],
            vec![TestBackend::chat_uri()],
        )
        .await
        .expect("initializes");
    let mut sub = client.attach_subscription(&TestBackend::chat_uri()).await;

    let handle = client
        .dispatch(
            TestBackend::chat_uri(),
            action(serde_json::json!({
                "type": "chat/pendingMessageRemoved",
                "kind": "steering",
                "id": "p-1",
            })),
        )
        .await
        .expect("dispatches");

    let event = tokio::time::timeout(Duration::from_secs(2), sub.recv())
        .await
        .expect("echo arrives")
        .expect("subscription open");
    let envelope = match event {
        ahp::SubscriptionEvent::Action(envelope) => envelope,
        other => panic!("expected an action envelope, got {other:?}"),
    };
    let origin = envelope
        .origin
        .expect("the originator's echo carries origin");
    assert_eq!(origin.client_id, "desktop");
    assert_eq!(origin.client_seq, handle.client_seq);
    assert!(envelope.rejection_reason.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn undeclared_action_is_echoed_with_a_rejection_reason() {
    let host = Host::new(TestBackend::new() as Arc<dyn Backend>);
    let client = connect(&host).await;
    client
        .initialize(
            "desktop".to_string(),
            vec![PROTOCOL_VERSION.to_string()],
            vec![ROOT_RESOURCE_URI.to_string()],
        )
        .await
        .expect("initializes");
    let mut sub = client.attach_subscription(ROOT_RESOURCE_URI).await;

    // `root/agentsChanged` is host-originated only: a client must not send it,
    // and it must learn that instead of being silently dropped.
    client
        .dispatch(
            ROOT_RESOURCE_URI.to_string(),
            StateAction::RootAgentsChanged(ahp_types::actions::RootAgentsChangedAction {
                agents: Vec::<AgentInfo>::new(),
            }),
        )
        .await
        .expect("dispatch is fire-and-forget");

    let event = tokio::time::timeout(Duration::from_secs(2), sub.recv())
        .await
        .expect("rejection arrives")
        .expect("subscription open");
    let envelope = match event {
        ahp::SubscriptionEvent::Action(envelope) => envelope,
        other => panic!("expected an action envelope, got {other:?}"),
    };
    assert!(envelope.rejection_reason.is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn unsubscribe_stops_delivery() {
    let host = Host::new(TestBackend::new() as Arc<dyn Backend>);
    let client = connect(&host).await;
    client
        .initialize(
            "desktop".to_string(),
            vec![PROTOCOL_VERSION.to_string()],
            vec![TestBackend::session_uri()],
        )
        .await
        .expect("initializes");
    let mut sub = client
        .attach_subscription(&TestBackend::session_uri())
        .await;

    client
        .unsubscribe(TestBackend::session_uri())
        .await
        .expect("unsubscribes");
    host.publish(
        &TestBackend::session_uri(),
        action(serde_json::json!({"type": "session/titleChanged", "title": "after unsubscribe"})),
        None,
    );

    let event = tokio::time::timeout(Duration::from_secs(1), sub.recv()).await;
    match event {
        Ok(None) => {}
        Ok(Some(other)) => panic!("delivery continued after unsubscribe: {other:?}"),
        Err(_) => panic!("subscription handle stayed open after unsubscribe"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn reconnect_answers_with_fresh_snapshots() {
    let host = Host::new(TestBackend::new() as Arc<dyn Backend>);

    let first = connect(&host).await;
    let init = first
        .initialize(
            "desktop".to_string(),
            vec![PROTOCOL_VERSION.to_string()],
            vec![ROOT_RESOURCE_URI.to_string()],
        )
        .await
        .expect("initializes");
    let last_seen = init.server_seq;
    drop(first);

    let second = connect(&host).await;
    let result = second
        .reconnect(
            "desktop".to_string(),
            last_seen,
            vec![ROOT_RESOURCE_URI.to_string(), TestBackend::session_uri()],
        )
        .await
        .expect("reconnects");

    match result {
        ahp_types::commands::ReconnectResult::Snapshot(snapshot) => {
            assert_eq!(snapshot.snapshots.len(), 2);
        }
        other => panic!("manox answers reconnection with snapshots, got {other:?}"),
    }
}

/// A dialect that renames one method on the way in and one on the way out —
/// the smallest proof that the per-connection seam is live in both directions.
struct RenamingDialect;

impl manox_ahp::connection::Dialect for RenamingDialect {
    fn outgoing(
        &self,
        message: ahp_types::messages::JsonRpcMessage,
    ) -> ahp_types::messages::JsonRpcMessage {
        match message {
            ahp_types::messages::JsonRpcMessage::Notification(mut note)
                if note.method == "root/sessionAdded" =>
            {
                note.method = "root/dialectSessionAdded".to_string();
                ahp_types::messages::JsonRpcMessage::Notification(note)
            }
            other => other,
        }
    }

    fn incoming(
        &self,
        message: ahp_types::messages::JsonRpcMessage,
    ) -> ahp_types::messages::JsonRpcMessage {
        match message {
            ahp_types::messages::JsonRpcMessage::Request(mut request)
                if request.method == "dialect/ping" =>
            {
                request.method = "ping".to_string();
                ahp_types::messages::JsonRpcMessage::Request(request)
            }
            other => other,
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn extension_channels_fold_their_own_state() {
    // AHP's reducers ignore private actions by construction (`Unknown` is
    // `OutOfScope` upstream), and `SnapshotState` has no arm for private state,
    // so the host folds `x-manox` state itself and delivers it as actions.
    let host = Host::new(TestBackend::new() as Arc<dyn Backend>);
    let client = connect(&host).await;
    client
        .initialize(
            "desktop".to_string(),
            vec![PROTOCOL_VERSION.to_string()],
            vec![],
        )
        .await
        .expect("initializes");
    let uri = "x-manox-plan:/c-1";
    let mut sub = client.attach_subscription(uri).await;
    // Subscribing is what a client does; the channel is stateless by design
    // (no snapshot), which the empty result states.
    let (result, _) = client.subscribe(uri.to_string()).await.expect("subscribes");
    assert!(
        result.snapshot.is_none(),
        "extension channels are stateless"
    );

    let envelope = host.publish(
        uri,
        action(serde_json::json!({
            "type": "x-manox-plan/planModeChanged",
            "enabled": true,
        })),
        None,
    );
    let delivered = tokio::time::timeout(Duration::from_secs(2), sub.recv())
        .await
        .expect("the delta arrives")
        .expect("subscription open");
    match delivered {
        ahp::SubscriptionEvent::Action(envelope) => {
            assert_eq!(envelope.server_seq, envelope.server_seq.max(1));
        }
        other => panic!("expected an action envelope, got {other:?}"),
    }
    assert_eq!(envelope.server_seq, 1);
    let state = host.extension_state(uri).expect("folded extension state");
    assert_eq!(state.plan_mode, Some(true));

    // An action this build does not know leaves the state alone.
    host.publish(
        uri,
        action(serde_json::json!({"type": "x-manox-session/ofTheFuture", "x": 1})),
        None,
    );
    let after = host.extension_state(uri).expect("state survives");
    assert_eq!(after, state);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_connection_dialect_rewrites_in_both_directions() {
    let host = Host::new(TestBackend::new() as Arc<dyn Backend>);
    let (host_side, client_side) = inproc::pair();
    // The seam is installed on the host side of the connection, exactly where a
    // client's dialect would be recognised.
    host.accept_with_dialect(host_side, Box::new(RenamingDialect));
    let client = ahp::Client::connect(client_side, ClientConfig::default())
        .await
        .expect("connects");
    client
        .initialize(
            "dialect".to_string(),
            vec![PROTOCOL_VERSION.to_string()],
            vec![],
        )
        .await
        .expect("initializes");

    // Inbound: the client's own method name reaches the host as the real one.
    client
        .notify(
            "dialect/ping",
            serde_json::json!({"channel": ROOT_RESOURCE_URI}),
        )
        .await
        .expect("dialect ping is accepted");
    assert!(
        client.ping().await.is_ok(),
        "the host is still answering after the dialect call"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn pre_creation_commands_answer_with_empty_shapes() {
    // The reference client calls `resolveSessionConfig` before it creates a
    // session and `completions` while the user types: both must answer, not
    // `MethodNotFound`, or the client never reaches `createSession`.
    let host = Host::new(TestBackend::new() as Arc<dyn Backend>);
    let client = connect(&host).await;
    client
        .initialize(
            "desktop".to_string(),
            vec![PROTOCOL_VERSION.to_string()],
            vec![],
        )
        .await
        .expect("initializes");

    let resolved: ahp_types::commands::ResolveSessionConfigResult = client
        .request(
            "resolveSessionConfig",
            ahp_types::commands::ResolveSessionConfigParams {
                channel: ROOT_RESOURCE_URI.to_string(),
                meta: None,
                provider: None,
                working_directory: None,
                config: None,
            },
        )
        .await
        .expect("resolveSessionConfig answers");
    assert_eq!(resolved.schema.r#type, "object");
    assert!(resolved.schema.properties.is_empty());
    assert!(resolved.values.is_empty());

    let completions: ahp_types::commands::CompletionsResult = client
        .request(
            "completions",
            ahp_types::commands::CompletionsParams {
                channel: TestBackend::chat_uri(),
                meta: None,
                kind: ahp_types::commands::CompletionItemKind::UserMessage,
                text: "@".to_string(),
                offset: 1,
            },
        )
        .await
        .expect("completions answers");
    assert!(completions.items.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn in_process_scenario_matches_the_expected_log() {
    let host = Host::new(TestBackend::new() as Arc<dyn Backend>);
    let (host_side, client_side) = inproc::pair();
    host.accept(host_side);

    let log = common::run_scenario(host, client_side).await;
    assert_eq!(log, common::EXPECTED_LOG);
}

#[tokio::test(flavor = "multi_thread")]
async fn list_sessions_pages_and_root_notifications_flow() {
    let host = Host::new(TestBackend::new() as Arc<dyn Backend>);
    let client = connect(&host).await;
    client
        .initialize(
            "desktop".to_string(),
            vec![PROTOCOL_VERSION.to_string()],
            vec![ROOT_RESOURCE_URI.to_string()],
        )
        .await
        .expect("initializes");
    let mut sub = client.attach_subscription(ROOT_RESOURCE_URI).await;

    let listed: ahp_types::commands::ListSessionsResult = client
        .request(
            "listSessions",
            ListSessionsParams {
                channel: root::URI.to_string(),
                meta: None,
                limit: None,
                cursor: None,
            },
        )
        .await
        .expect("lists sessions");
    assert_eq!(listed.items.len(), 1);
    assert_eq!(listed.items[0].resource, TestBackend::session_uri());
    assert!(listed.next_cursor.is_none());

    host.summary_changed(
        "s-1",
        ahp_types::notifications::PartialSessionSummary {
            title: Some("renamed".to_string()),
            ..Default::default()
        },
    );
    let event = tokio::time::timeout(Duration::from_secs(2), sub.recv())
        .await
        .expect("notification arrives")
        .expect("subscription open");
    match event {
        ahp::SubscriptionEvent::SessionSummaryChanged(params) => {
            assert_eq!(params.session, TestBackend::session_uri());
            assert_eq!(params.changes.title.as_deref(), Some("renamed"));
        }
        other => panic!("expected a summary change, got {other:?}"),
    }
}
