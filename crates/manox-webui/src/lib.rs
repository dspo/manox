//! WebUI host surface: an in-process HTTP + WebSocket server that serves the
//! shared webview bundle to a browser.
//!
//! The browser is a dumb terminal — every command is driven on the app main
//! thread against the same `Entity<Thread>`s the desktop Workspace operates,
//! through the `AgentServer` protocol gateway. The `pump` is a gpui-foreground
//! task that polls per-connection command queues; the HTTP and WS workers run
//! on the global tokio runtime (`manox_agent::runtime::handle`). Each connection is
//! one independent bridge, matching the vscode multi-surface model; on
//! disconnect it detaches its sessions without
//! cancelling turns, so a browser refresh never kills a desktop turn.

mod bridge;
mod proto_translate;
mod pump;
mod server;

use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{Value, json};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::bridge::Outbound;

pub use pump::spawn_pump;

/// The foreground pump's inbound channel: the server registers/tears down
/// connections here; the pump holds the receiver on the main thread.
pub(crate) static MAIN_CHANNEL: OnceLock<Mutex<Option<UnboundedSender<ToMain>>>> = OnceLock::new();

/// Server lifecycle: never started, starting, or serving with a URL.
static STATE: OnceLock<Mutex<ServerState>> = OnceLock::new();

struct ServerState {
    service: Option<Arc<WebuiService>>,
    starting: bool,
    pending_open: bool,
}

/// The running service endpoint. Owned by the server task; shared with the
/// tray so a second open focuses the existing browser tab.
pub struct WebuiService {
    pub url: String,
    pub port: u16,
    token: String,
}

pub(crate) enum ToMain {
    Connect(ConnectionHandle),
    Disconnect(u64),
}

/// Everything the pump needs to own one connection's command/event channels.
pub(crate) struct ConnectionHandle {
    pub id: u64,
    pub cmd_rx: UnboundedReceiver<Value>,
    pub outbound: Arc<Outbound>,
}

fn state() -> std::sync::MutexGuard<'static, ServerState> {
    STATE
        .get_or_init(|| {
            Mutex::new(ServerState {
                service: None,
                starting: false,
                pending_open: false,
            })
        })
        .lock()
        .unwrap()
}

/// Tray entry: open the browser tab, starting the server on first use. A
/// second click while the server is coming up marks a pending open instead of
/// starting a second listener.
pub fn open_webui() {
    let mut st = state();
    if let Some(svc) = &st.service {
        let url = svc.url.clone();
        drop(st);
        open_url(&url);
        return;
    }
    if st.starting {
        st.pending_open = true;
        return;
    }
    st.starting = true;
    st.pending_open = true;
    drop(st);
    start_server();
}

fn start_server() {
    let handle = manox_agent::runtime::handle();
    handle.spawn(async move {
        if let Err(e) = server::bind_and_serve().await {
            let mut st = state();
            st.starting = false;
            st.pending_open = false;
            tracing::error!(error = %e, "webui server failed to start");
        }
    });
}

/// The server task calls this once its listener is bound: record the
/// endpoint, persist `webui.json`, and honor a pending open.
pub(crate) fn service_started(svc: WebuiService) {
    let (open_it, url, token, port) = {
        let mut st = state();
        st.service = Some(Arc::new(svc));
        st.starting = false;
        let open_it = st.pending_open;
        st.pending_open = false;
        let svc = st.service.as_ref().unwrap();
        (open_it, svc.url.clone(), svc.token.clone(), svc.port)
    };
    write_webui_json(&token, port, &url);
    if open_it {
        open_url(&url);
    }
}

/// Persist the endpoint so out-of-process tooling can reach the service;
/// mode 0600 keeps the token local to the user.
fn write_webui_json(token: &str, port: u16, url: &str) {
    let Ok(dir) = manox_agent::paths::manox_config_dir() else {
        return;
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(error = %e, "failed to create manox config dir for webui.json");
        return;
    }
    let payload = json!({"port": port, "token": token, "url": url});
    let path = dir.join("webui.json");
    if let Err(e) = std::fs::write(&path, payload.to_string()) {
        tracing::warn!(error = %e, path = %path.display(), "failed to write webui.json");
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
}

fn open_url(url: &str) {
    if let Err(e) = open::that(url) {
        tracing::warn!(error = %e, url, "failed to open webui url");
    }
}

/// The server clones the pump's sender so WS workers can register
/// connections. `None` before `spawn_pump` runs (app startup guarantees the
/// pump is live before any tray open).
pub(crate) fn main_channel_sender() -> Option<UnboundedSender<ToMain>> {
    MAIN_CHANNEL.get().and_then(|m| m.lock().unwrap().clone())
}
