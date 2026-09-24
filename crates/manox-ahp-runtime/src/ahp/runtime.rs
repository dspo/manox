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
use crate::runtime_trait::SessionRuntime;

/// The process-singleton AHP runtime (L11).
static RUNTIME: OnceLock<Arc<AhpRuntime>> = OnceLock::new();

/// How the process builds its runtime, installed once by whichever embedder
/// owns the session store.
///
/// The runtime half cannot construct a session runtime itself: that is the
/// gateway's (or a future replacement's) business, and naming a concrete one
/// here would recreate the dependency this crate exists to avoid. So the owner
/// installs a constructor before the first [`runtime`] call.
static BUILDER: OnceLock<fn(PathBuf) -> Arc<dyn crate::runtime_trait::SessionRuntime>> =
    OnceLock::new();

/// Install the constructor for the process-singleton runtime.
///
/// Last-wins is deliberately **not** offered: a process has one session store,
/// and letting two embedders race for it would give the AHP face a runtime other
/// than the one its clients are talking to. A second call is refused loudly.
pub fn install_builder(
    builder: fn(PathBuf) -> Arc<dyn crate::runtime_trait::SessionRuntime>,
) -> Result<(), &'static str> {
    BUILDER
        .set(builder)
        .map_err(|_| "an AHP runtime builder is already installed for this process")
}

/// The AHP host plus the runtime adapter behind it.
pub struct AhpRuntime {
    host: Arc<Host>,
}

impl AhpRuntime {
    /// Build a runtime over an explicit server (the tests' entry point; the
    /// process-wide one is [`runtime`]).
    pub fn new(server: Arc<dyn SessionRuntime>, cwd: PathBuf) -> Arc<Self> {
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

impl AhpRuntime {
    /// Whether any of this session's AHP subscribers declared it can answer
    /// `method` — the gate the capability router uses to decide whether this
    /// transport has a client for the request at all.
    pub fn has_capable_client(&self, session_id: &str, method: &str) -> bool {
        self.host.has_capable_client(session_id, method)
    }

    /// Ask one such client (see [`manox_ahp::Host::request_client`]).
    pub async fn request_client(
        &self,
        session_id: &str,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, manox_ahp::error::HostError> {
        self.host.request_client(session_id, method, params).await
    }
}

/// The process-singleton runtime, when it has been initialized.
///
/// `None` in a host that never brought the AHP face up — a headless or napi
/// embedder, or a test that only exercises the v2 path. Callers treat that as
/// "this transport has no client", never as an error.
pub fn try_runtime() -> Option<Arc<AhpRuntime>> {
    RUNTIME.get().map(Arc::clone)
}

/// The process-singleton runtime (L11). `cwd` only matters on first call, where
/// it seeds the agent server identity the v2 hosts also use.
pub fn runtime(cwd: PathBuf) -> Arc<AhpRuntime> {
    Arc::clone(RUNTIME.get_or_init(|| {
        let build = *BUILDER.get().expect(
            "no AHP runtime builder installed: the embedder that owns the session \
             store must call `install_builder` before the AHP face is served",
        );
        AhpRuntime::new(build(cwd.clone()), cwd)
    }))
}
