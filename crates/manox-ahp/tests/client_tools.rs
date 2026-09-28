//! Client-contributed session tools over the real host path.
//!
//! AHP models a client-contributed tool set natively — a client dispatches
//! `session/activeClientSet` carrying its own `SessionActiveClient`, whose
//! `tools` field is the contribution — so these tests pin *both* halves of that
//! claim:
//!
//! 1. the registration reaches the runtime backend (so the model can be offered
//!    the tool at all), and
//! 2. it appears where the protocol says it should, at
//!    `SessionState.activeClients[].tools`, because the host folds the standard
//!    action through the upstream reducer rather than a private table.
//!
//! The two are asserted together on purpose: serving only the second would fold
//! a tool set no model can call, and serving only the first would leave every
//! subscriber unable to see what the session's clients contribute.
//!
//! Every registration here travels the real write path — a subscribed client
//! dispatching the action — because that is the only path that reaches
//! [`manox_ahp::Backend::dispatch`]; `Host::publish` is the host-originated leg
//! and would bypass the very routing under test.

mod common;

use std::sync::Arc;
use std::time::Duration;

use ahp::{Client, ClientConfig};
use ahp_types::actions::{ActionEnvelope, SessionActiveClientSetAction, StateAction};
use ahp_types::common::ROOT_RESOURCE_URI;
use ahp_types::state::{SessionActiveClient, ToolAnnotations, ToolDefinition};
use ahp_types::version::PROTOCOL_VERSION;
use manox_ahp::Host;
use manox_ahp::backend::Backend;
use manox_ahp::transport::inproc;

use common::{RecordingBackend, TestBackend};

fn connect(host: &Host) -> impl std::future::Future<Output = Client> {
    let (host_side, client_side) = inproc::pair();
    host.accept(host_side);
    async move {
        Client::connect(client_side, ClientConfig::default())
            .await
            .expect("connects")
    }
}

/// One contributed tool, built typed: a malformed JSON literal would fall into
/// `StateAction::Unknown` and the test would then assert about the wrong thing.
fn tool(name: &str) -> ToolDefinition {
    ToolDefinition {
        name: name.to_string(),
        title: None,
        description: Some(format!("the {name} tool")),
        input_schema: Some(serde_json::json!({
            "type": "object",
            "properties": { "q": { "type": "string" } },
        })),
        output_schema: None,
        annotations: None,
        meta: None,
    }
}

fn registration(client_id: &str, tools: Vec<ToolDefinition>) -> StateAction {
    StateAction::SessionActiveClientSet(SessionActiveClientSetAction {
        active_client: SessionActiveClient {
            client_id: client_id.to_string(),
            display_name: Some("Test Client".to_string()),
            tools,
            customizations: None,
        },
    })
}

/// The envelope the host echoes for a dispatched action.
async fn next_envelope(sub: &mut ahp::SessionSubscription) -> ActionEnvelope {
    let event = tokio::time::timeout(Duration::from_secs(2), sub.recv())
        .await
        .expect("echo arrives")
        .expect("subscription open");
    match event {
        ahp::SubscriptionEvent::Action(envelope) => envelope,
        other => panic!("expected an action envelope, got {other:?}"),
    }
}

/// A connected, initialized client subscribed to the seeded session.
///
/// A real `subscribe`, not `attach_subscription`: the host refuses a dispatch on
/// a channel the connection has not subscribed to, so a local-only attachment
/// would test the refusal path rather than registration.
async fn subscribed(host: &Host) -> (Client, ahp::SessionSubscription) {
    let client = connect(host).await;
    client
        .initialize(
            "desktop".to_string(),
            vec![PROTOCOL_VERSION.to_string()],
            vec![ROOT_RESOURCE_URI.to_string(), TestBackend::session_uri()],
        )
        .await
        .expect("initializes");
    let sub = client
        .attach_subscription(&TestBackend::session_uri())
        .await;
    (client, sub)
}

/// A host whose backend records what it was asked to do, with `s-1` seeded.
fn fixture() -> (Host, Arc<RecordingBackend>) {
    let backend = RecordingBackend::new();
    let host = Host::new(Arc::clone(&backend) as Arc<dyn Backend>);
    (host, backend)
}

/// Dispatch one registration over the wire and return the host's echo.
async fn register(
    client: &Client,
    sub: &mut ahp::SessionSubscription,
    action: StateAction,
) -> ActionEnvelope {
    client
        .dispatch(TestBackend::session_uri(), action)
        .await
        .expect("dispatch is fire-and-forget");
    next_envelope(sub).await
}

/// Registration reaches the runtime *and* the protocol state: the two halves
/// of the capability, asserted on one dispatched action.
#[tokio::test(flavor = "multi_thread")]
async fn active_client_set_registers_tools_on_the_runtime_and_the_session_state() {
    let (host, backend) = fixture();
    let (client, mut sub) = subscribed(&host).await;

    let envelope = register(
        &client,
        &mut sub,
        registration("desktop", vec![tool("get_selection"), tool("read_file")]),
    )
    .await;
    assert!(
        envelope.rejection_reason.is_none(),
        "registration was refused: {:?}",
        envelope.rejection_reason
    );

    // Half one: the runtime saw the tools, under the client that contributed
    // them. A host that only folded would leave this empty.
    let sets = backend.active_client_sets();
    assert_eq!(sets.len(), 1, "the registration reached the backend");
    assert_eq!(sets[0].client_id, "desktop");
    assert_eq!(
        sets[0]
            .tools
            .iter()
            .map(|t| t.name.as_str())
            .collect::<Vec<_>>(),
        vec!["get_selection", "read_file"]
    );

    // Half two: the protocol state carries them where AHP says it should.
    let state = host
        .session_state("s-1")
        .expect("the session is seeded by the subscribe");
    assert_eq!(state.active_clients.len(), 1);
    assert_eq!(state.active_clients[0].client_id, "desktop");
    assert_eq!(
        state.active_clients[0]
            .tools
            .iter()
            .map(|t| t.name.as_str())
            .collect::<Vec<_>>(),
        vec!["get_selection", "read_file"]
    );
}

/// The echo carries the originator's identity: a client can attribute the
/// accepted write to itself rather than inferring it from the state.
#[tokio::test(flavor = "multi_thread")]
async fn the_accepted_registration_echoes_its_origin() {
    let (host, _backend) = fixture();
    let (client, mut sub) = subscribed(&host).await;

    let envelope = register(
        &client,
        &mut sub,
        registration("desktop", vec![tool("get_selection")]),
    )
    .await;
    let origin = envelope.origin.expect("the echo carries the origin");
    assert_eq!(origin.client_id, "desktop");
}

/// Re-registering replaces the client's set rather than appending: the upsert
/// is keyed by `clientId`, and an append would hand the model stale tools.
#[tokio::test(flavor = "multi_thread")]
async fn re_registration_replaces_the_previous_set() {
    let (host, backend) = fixture();
    let (client, mut sub) = subscribed(&host).await;

    register(
        &client,
        &mut sub,
        registration("desktop", vec![tool("old")]),
    )
    .await;
    register(
        &client,
        &mut sub,
        registration("desktop", vec![tool("new")]),
    )
    .await;

    assert_eq!(backend.active_client_sets().len(), 2);
    let state = host.session_state("s-1").expect("seeded");
    assert_eq!(
        state.active_clients.len(),
        1,
        "one entry per clientId, whatever the re-registration count"
    );
    assert_eq!(state.active_clients[0].tools.len(), 1);
    assert_eq!(state.active_clients[0].tools[0].name, "new");
}

/// Two clients keep their own entries: `activeClients` is keyed by `clientId`,
/// so neither evicts the other.
#[tokio::test(flavor = "multi_thread")]
async fn two_clients_keep_separate_registrations() {
    let (host, backend) = fixture();
    let (client, mut sub) = subscribed(&host).await;

    register(&client, &mut sub, registration("alpha", vec![tool("a")])).await;
    register(&client, &mut sub, registration("beta", vec![tool("b")])).await;

    let state = host.session_state("s-1").expect("seeded");
    let mut ids: Vec<&str> = state
        .active_clients
        .iter()
        .map(|c| c.client_id.as_str())
        .collect();
    ids.sort();
    assert_eq!(ids, vec!["alpha", "beta"]);
    assert_eq!(backend.active_client_sets().len(), 2);
}

/// The tool's schema and side-effect hint cross the protocol intact — a
/// registration that lost either would mount a tool the model cannot fill in,
/// or gate a read-only contributor.
#[tokio::test(flavor = "multi_thread")]
async fn the_tool_definition_survives_the_round_trip() {
    let (host, _backend) = fixture();
    let (client, mut sub) = subscribed(&host).await;

    let mut annotated = tool("reader");
    annotated.annotations = Some(ToolAnnotations {
        title: Some("Reader".to_string()),
        read_only_hint: Some(true),
        destructive_hint: None,
        idempotent_hint: None,
        open_world_hint: None,
    });
    register(&client, &mut sub, registration("desktop", vec![annotated])).await;

    let state = host.session_state("s-1").expect("seeded");
    let mounted = &state.active_clients[0].tools[0];
    assert_eq!(mounted.name, "reader");
    assert_eq!(mounted.description.as_deref(), Some("the reader tool"));
    assert_eq!(
        mounted.input_schema,
        Some(serde_json::json!({
            "type": "object",
            "properties": { "q": { "type": "string" } },
        }))
    );
    assert_eq!(
        mounted.annotations.as_ref().and_then(|a| a.read_only_hint),
        Some(true)
    );
}

/// A registration the runtime refuses is echoed back as rejected, never as an
/// accepted write: the client must be able to tell that its tools did not land.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_registration_is_echoed_with_a_rejection_reason() {
    let (host, backend) = fixture();
    let (client, mut sub) = subscribed(&host).await;

    // The runtime's refusal must reach the wire verbatim.
    backend.refuse_with("unknown session: s-1");
    let envelope = register(
        &client,
        &mut sub,
        registration("desktop", vec![tool("get_selection")]),
    )
    .await;
    assert_eq!(
        envelope.rejection_reason.as_deref(),
        Some("unknown session: s-1")
    );
    // The refused write must not have been folded into the shared state.
    let state = host.session_state("s-1").expect("seeded");
    assert!(
        state.active_clients.is_empty(),
        "a refused registration must not appear in the state subscribers read"
    );
}

/// The acceptance table is the gate, and it lists the registration action: a
/// host that folds `activeClients` but never accepts `session/activeClientSet`
/// could not receive them.
#[test]
fn the_registration_action_is_on_the_accepted_table() {
    assert!(
        manox_ahp::ext::accepts_action("ahp-session:/s-1", "session/activeClientSet"),
        "session/activeClientSet must be accepted on a session channel"
    );
    // Channel scoping is part of the gate: a session action on a chat channel
    // is a client bug, and admitting it would route the write to the wrong
    // channel.
    assert!(!manox_ahp::ext::accepts_action(
        "ahp-chat:/c-1",
        "session/activeClientSet"
    ));
}

/// The declared surface and the served surface must not drift: this host
/// registers client tools through the standard action, so it must **not**
/// advertise an `x-manox/registerSessionTools` command it does not answer.
#[test]
fn the_extension_declaration_does_not_advertise_a_registration_command() {
    let declaration = manox_ahp::ext::declaration();
    let commands = declaration["commands"]
        .as_array()
        .expect("commands are declared")
        .iter()
        .filter_map(|c| c.as_str())
        .collect::<Vec<_>>();
    assert!(
        !commands.contains(&"x-manox/registerSessionTools"),
        "client tools are an AHP-native action; a second channel would be a \
         second source of truth (declared: {commands:?})"
    );
    assert_eq!(
        manox_ahp::ext::CLIENT_TOOLS_VIA_ACTION,
        "session/activeClientSet"
    );
    let accepted = declaration["acceptedActions"]
        .as_array()
        .expect("accepted actions are declared")
        .iter()
        .filter_map(|a| a.as_str())
        .collect::<Vec<_>>();
    assert!(
        accepted.contains(&manox_ahp::ext::CLIENT_TOOLS_VIA_ACTION),
        "the declaration must list the action it accepts for client tools"
    );
}

/// A session the runtime does not know is refused at the subscribe, so a
/// registration can never be folded into a session that will not mount it.
#[tokio::test(flavor = "multi_thread")]
async fn registration_on_an_unknown_session_is_refused() {
    let (host, backend) = fixture();
    let client = connect(&host).await;
    client
        .initialize(
            "desktop".to_string(),
            vec![PROTOCOL_VERSION.to_string()],
            vec![ROOT_RESOURCE_URI.to_string()],
        )
        .await
        .expect("initializes");

    // `s-missing` is not a session this backend knows, so the host cannot seed
    // it — and subscribing is how the client learns that.
    let missing = manox_ahp::channels::session::uri("s-missing");
    assert!(
        client.subscribe(missing).await.is_err(),
        "an unknown session is refused at subscribe, not served empty"
    );
    // Nothing was routed to the runtime, so it was never asked to honour a
    // write against a session it does not drive.
    assert!(backend.dispatched().is_empty());
    assert!(host.session_state("s-missing").is_none());
}
