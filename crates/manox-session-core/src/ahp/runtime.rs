//! The process-singleton AHP runtime — one host, two transports.
//!
//! L11 survives the protocol change: a process owns exactly one AHP host, so
//! there is one `serverSeq` domain, one channel store and one set of
//! subscriptions. Frontends differ only in how they reach it:
//!
//! - **over channel** — the in-process transport ([`AhpRuntime::inproc`]): the
//!   desktop app's leg. Typed frames cross an unbounded channel pair; nothing is
//!   serialized and no port is opened.
//! - **over websocket** — the gateway's `/ahp` route ([`AhpRuntime::router`]),
//!   mounted on the same loopback listener (and the same token/lock/singleton
//!   discipline) as the retiring v2 `/ws` route. Remote clients, VS Code's agent
//!   window and AHPX all come in this way.
//!
//! Both legs serve the same [`Host`], so a state change published once reaches
//! an in-process subscriber and a WebSocket subscriber with the same
//! `serverSeq` — that parity is asserted in `tests/ahp_transport_parity.rs`.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use manox_ahp::Host;
use manox_ahp::transport::HostTransport;
use manox_ahp::transport::inproc::InprocClientTransport;

use super::backend::RuntimeBackend;
use crate::agent_server::AgentServer;

/// The AHP host plus the runtime adapter behind it.
pub struct AhpRuntime {
    host: Arc<Host>,
}

impl AhpRuntime {
    /// Build a runtime over an explicit server (the tests' entry point; the
    /// process-wide one is [`runtime`]).
    pub fn new(server: Arc<AgentServer>, cwd: PathBuf) -> Arc<Self> {
        let backend = RuntimeBackend::new(Arc::clone(&server), cwd);
        let host = Arc::new(Host::new(
            Arc::clone(&backend) as Arc<dyn manox_ahp::Backend>
        ));
        backend.attach_host(&host);
        let runtime = Arc::new(Self {
            host: Arc::clone(&host),
        });
        // The provider registry loads asynchronously; the root channel's agent
        // catalogue is state, so it arrives as an action once it is ready rather
        // than as a snapshot that may be born empty.
        manox_agent::runtime::handle().spawn(async move {
            manox_agent::provider_glue::wait_ready().await;
            let agents = host.backend().root_state().agents;
            if agents.is_empty() {
                return;
            }
            host.publish(
                ahp_types::common::ROOT_RESOURCE_URI,
                ahp_types::actions::StateAction::RootAgentsChanged(
                    ahp_types::actions::RootAgentsChangedAction { agents },
                ),
                None,
            );
        });
        runtime
    }

    /// The one AHP host of this process.
    pub fn host(&self) -> Arc<Host> {
        Arc::clone(&self.host)
    }

    /// Connect one **in-process** client ("over channel") and hand back the
    /// client end; the host end is accepted here.
    pub fn inproc(&self) -> InprocClientTransport {
        let (host_side, client_side) = manox_ahp::transport::inproc::pair();
        self.host.accept(host_side);
        client_side
    }

    /// Accept an already-framed transport (the WebSocket route uses this).
    pub fn accept(&self, transport: HostTransport) {
        self.host.accept(transport);
    }

    /// The gateway router carrying `/ahp` alongside the v2 route.
    #[cfg(feature = "ws-gateway")]
    pub fn router(&self, path: &str) -> axum::Router {
        manox_ahp::transport::axum_ws::router(path, (*self.host()).clone())
    }
}

/// The process-singleton runtime (L11). `cwd` only matters on first call, where
/// it seeds the agent server identity the v2 hosts also use.
pub fn runtime(cwd: PathBuf) -> Arc<AhpRuntime> {
    static RUNTIME: OnceLock<Arc<AhpRuntime>> = OnceLock::new();
    Arc::clone(RUNTIME.get_or_init(|| {
        let server = crate::agent_server::global(cwd.clone());
        AhpRuntime::new(server, cwd)
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ahp::ClientConfig;
    use ahp_types::version::PROTOCOL_VERSION;

    /// Both legs serve one host: a root action published once arrives at the
    /// in-process subscriber and the WebSocket subscriber with the same
    /// `serverSeq` — the property that lets a local window and a remote client
    /// share a session without a second gateway in the process.
    #[tokio::test(flavor = "multi_thread")]
    async fn in_process_and_websocket_legs_share_one_host() {
        // The crate's established test scaffolding (same shape as the fold
        // suite): a hermetic HOME, a standalone threads db and a scratch
        // registry file, guarded so the overrides cannot leak across suites.
        let outer = crate::test_support::lock_globals();
        let store_lock = manox_agent::thread_store::store_test_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        crate::test_support::hermetic_home();
        crate::test_support::init_globals();
        let scratch = std::env::temp_dir().join(format!(
            "manox-ahp-parity-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&scratch).expect("scratch dir");
        let db = Arc::new(
            manox_agent::db::ThreadsDatabase::open(&scratch.join("threads.db"))
                .expect("threads db"),
        );
        manox_agent::thread_store::init_for_test(db);
        manox_agent::thread_registry::set_registry_path_for_test(Some(
            scratch.join("threads.registry.json"),
        ));
        let _guards = (outer, store_lock);

        let cwd = manox_agent::paths::home_dir().unwrap_or_else(|| PathBuf::from("."));
        let server = Arc::new(AgentServer::new_without_store_watcher(cwd.clone()));
        let runtime = AhpRuntime::new(Arc::clone(&server), cwd);

        // Leg one: over channel (in-process).
        let inproc = ahp::Client::connect(runtime.inproc(), ClientConfig::default())
            .await
            .expect("in-process client connects");
        let inproc_init = inproc
            .initialize(
                "desktop".to_string(),
                vec![PROTOCOL_VERSION.to_string()],
                vec![ahp_types::common::ROOT_RESOURCE_URI.to_string()],
            )
            .await
            .expect("in-process initialize");
        let mut inproc_root = inproc
            .attach_subscription(ahp_types::common::ROOT_RESOURCE_URI)
            .await;

        // Leg two: over websocket, through the gateway-shaped router.
        let app = runtime.router("/ahp");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("binds loopback");
        let addr = listener.local_addr().expect("bound address");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let client_transport = ahp_ws::WebSocketTransport::connect(&format!("ws://{addr}/ahp"))
            .await
            .expect("websocket client connects");
        let remote = ahp::Client::connect(client_transport, ClientConfig::default())
            .await
            .expect("websocket client ready");
        let remote_init = remote
            .initialize(
                "remote".to_string(),
                vec![PROTOCOL_VERSION.to_string()],
                vec![ahp_types::common::ROOT_RESOURCE_URI.to_string()],
            )
            .await
            .expect("websocket initialize");
        let mut remote_root = remote
            .attach_subscription(ahp_types::common::ROOT_RESOURCE_URI)
            .await;

        // Same host, same snapshot.
        assert_eq!(
            serde_json::to_value(&inproc_init.snapshots[0].state).unwrap(),
            serde_json::to_value(&remote_init.snapshots[0].state).unwrap(),
        );
        let meta = remote_init
            .meta
            .expect("the extension surface is advertised");
        assert_eq!(meta["x-manox"]["version"], 1);

        // One publish, two subscribers, one sequence number.
        let published = runtime.host().publish(
            ahp_types::common::ROOT_RESOURCE_URI,
            ahp_types::actions::StateAction::RootAgentsChanged(
                ahp_types::actions::RootAgentsChangedAction {
                    agents: runtime.host().backend().root_state().agents,
                },
            ),
            None,
        );
        let inproc_envelope = next_action(&mut inproc_root).await;
        let remote_envelope = next_action(&mut remote_root).await;
        assert_eq!(inproc_envelope.server_seq, published.server_seq);
        assert_eq!(remote_envelope.server_seq, published.server_seq);

        manox_agent::thread_registry::set_registry_path_for_test(None);
        manox_agent::thread_store::drop_for_test();
    }

    async fn next_action(sub: &mut ahp::SessionSubscription) -> ahp_types::actions::ActionEnvelope {
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), sub.recv())
            .await
            .expect("action arrives")
            .expect("subscription open");
        match event {
            ahp::SubscriptionEvent::Action(envelope) => envelope,
            other => panic!("expected an action envelope, got {other:?}"),
        }
    }
}
