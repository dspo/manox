//! Process-global LSP registry: PATH detection at startup, lazy per-`(spec,
//! root)` client lifecycle.
//!
//! Mirrors `agent::mcp::registry`'s `OnceLock` shape. `init()` probes each
//! executable with a bounded version command, but never spawns a long-running
//! server. Per-`(spec_id, root)` slot, `ensure` kicks off a
//! detached spawn+initialize (non-blocking — returns `Starting` immediately),
//! `wait_ready` blocks on a `Notify` with a caller-chosen timeout, and
//! `client_for` is the convenience path code-intel tools use (ensure +
//! bounded wait, returns the `Ready` client or an "indexing, retry" error).
//!
//! The `AgentTool` adapters that surface this as tools live in the `agent`
//! crate (`agent::lsp`) to avoid a dependency cycle (`agent` depends on
//! `lsp`; `lsp` must not depend on `agent`).

use std::collections::HashMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::{Mutex as StdMutex, OnceLock};
use std::time::Duration;

use anyhow::anyhow;
use tokio::sync::{Mutex, Notify};
use tracing::warn;

use crate::client::{LspClient, ServerStatus};
use crate::spec::{LspServerSpec, SPECS, spec_for_extension, spec_for_id};

static REGISTRY: OnceLock<LspRegistry> = OnceLock::new();

/// Bounded wait a code-intel tool does when the server isn't ready yet. Long
/// enough for a warm `initialize` to finish, short enough that the agent
/// doesn't stall on a wedged server.
const CODE_INTEL_WAIT: Duration = Duration::from_secs(5);

/// Per-`(spec_id, root)` lifecycle. The state transitions
/// `Idle → Starting → Ready | Failed`, driven by a detached init task; `Failed`
/// clears back to `Idle` on the next `ensure` so a transient failure retries.
struct ClientSlot {
    state: Mutex<SlotState>,
    notify: Arc<Notify>,
}

enum SlotState {
    Idle,
    Starting,
    Ready(Arc<LspClient>),
    Failed(String),
}

/// Lazy per-key cell.
type SlotMap = StdMutex<HashMap<(String, PathBuf), Arc<ClientSlot>>>;

pub struct LspRegistry {
    /// Specs whose executable passed both PATH detection and a viability probe.
    available: Vec<&'static LspServerSpec>,
    availability: HashMap<&'static str, ServerAvailability>,
    cells: SlotMap,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerAvailability {
    Available,
    NotInstalled,
    Broken(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerReport {
    pub id: &'static str,
    pub status: ServerStatus,
    pub detail: Option<String>,
}

impl LspRegistry {
    pub fn available_specs(&self) -> &[&'static LspServerSpec] {
        &self.available
    }

    pub fn is_available(&self, spec_id: &str) -> bool {
        self.available.iter().any(|s| s.id == spec_id)
    }

    pub fn availability(&self, spec_id: &str) -> Option<&ServerAvailability> {
        self.availability.get(spec_id)
    }

    /// Look up the spec for a file path's extension, among available servers.
    pub fn spec_for_path(&self, path: &Path) -> Option<&'static LspServerSpec> {
        let ext = path.extension()?.to_str()?;
        self.available
            .iter()
            .copied()
            .find(|s| s.extensions.contains(&ext))
    }

    /// Route by extension even when the corresponding executable is missing or
    /// broken, so callers can return the real installation/probe failure rather
    /// than the misleading "unsupported language" fallback.
    pub fn routed_spec_for_path(&self, path: &Path) -> Option<&'static LspServerSpec> {
        let ext = path.extension()?.to_str()?;
        spec_for_extension(ext)
    }

    pub fn workspace_root_for(
        &self,
        spec: &LspServerSpec,
        path: &Path,
        fallback: &Path,
    ) -> PathBuf {
        let start = if path.is_dir() {
            path
        } else {
            path.parent().unwrap_or(path)
        };
        for ancestor in start.ancestors() {
            if spec
                .root_hints
                .iter()
                .any(|hint| ancestor.join(hint).exists())
            {
                return ancestor.to_path_buf();
            }
        }
        fallback.to_path_buf()
    }

    pub async fn client_for_path(
        &self,
        path: &Path,
        fallback_root: &Path,
    ) -> anyhow::Result<Arc<LspClient>> {
        let Some(spec) = self.routed_spec_for_path(path) else {
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            return Err(anyhow!("no LSP server handles `.{ext}`"));
        };
        match self.availability(spec.id) {
            Some(ServerAvailability::Available) => {}
            Some(ServerAvailability::NotInstalled) | None => {
                return Err(anyhow!(
                    "LSP server `{}` is not installed; install it to enable code intelligence",
                    spec.id
                ));
            }
            Some(ServerAvailability::Broken(reason)) => {
                return Err(anyhow!(
                    "LSP server `{}` is installed but unusable: {reason}",
                    spec.id
                ));
            }
        }
        let root = self.workspace_root_for(spec, path, fallback_root);
        self.client_for(spec.id, root).await
    }

    /// Fire-and-forget callers use this to warm the correctly rooted server
    /// after the first source read without spending a model tool call.
    pub async fn ensure_for_path(
        &self,
        path: &Path,
        fallback_root: &Path,
    ) -> anyhow::Result<ServerStatus> {
        let Some(spec) = self.routed_spec_for_path(path) else {
            return Ok(ServerStatus::NotStarted);
        };
        if !self.is_available(spec.id) {
            return Ok(ServerStatus::NotStarted);
        }
        let root = self.workspace_root_for(spec, path, fallback_root);
        self.ensure(spec.id, root).await
    }

    /// Get-or-create the slot for `(spec_id, root)`.
    fn slot_for(&self, spec_id: &str, root: PathBuf) -> Arc<ClientSlot> {
        let key = (spec_id.to_string(), root);
        let mut map = self.cells.lock().expect("cells mutex poisoned");
        map.entry(key)
            .or_insert_with(|| {
                Arc::new(ClientSlot {
                    state: Mutex::new(SlotState::Idle),
                    notify: Arc::new(Notify::new()),
                })
            })
            .clone()
    }

    /// Non-blocking kick: spawn+initialize the server for `(spec_id, root)` if
    /// not already done, returning the current status. `NotStarted` means the
    /// server binary is not on `PATH` (nothing to spawn).
    pub async fn ensure(&self, spec_id: &str, root: PathBuf) -> anyhow::Result<ServerStatus> {
        let Some(spec) = spec_for_id(spec_id) else {
            return Err(anyhow!("unknown LSP server `{spec_id}`"));
        };
        if !self.is_available(spec_id) {
            return Ok(ServerStatus::NotStarted);
        }
        let slot = self.slot_for(spec_id, root.clone());
        let need_spawn = {
            let mut st = slot.state.lock().await;
            match &*st {
                SlotState::Ready(_) => return Ok(ServerStatus::Ready),
                SlotState::Starting => return Ok(ServerStatus::Starting),
                SlotState::Idle => {
                    *st = SlotState::Starting;
                    true
                }
                SlotState::Failed(prev) => {
                    warn!(
                        spec = spec_id,
                        prev, "re-attempting LSP spawn after previous failure"
                    );
                    *st = SlotState::Starting;
                    true
                }
            }
        };
        if need_spawn {
            // Detached: the caller sees `Starting` and decides whether to wait.
            let slot = slot.clone();
            tokio::spawn(async move {
                match LspClient::start(spec, root.clone()).await {
                    Ok(client) => match client.initialize().await {
                        Ok(()) => {
                            let mut st = slot.state.lock().await;
                            *st = SlotState::Ready(client);
                            slot.notify.notify_waiters();
                        }
                        Err(e) => {
                            client.mark_failed();
                            let mut st = slot.state.lock().await;
                            *st = SlotState::Failed(e.to_string());
                            slot.notify.notify_waiters();
                        }
                    },
                    Err(e) => {
                        let mut st = slot.state.lock().await;
                        *st = SlotState::Failed(e.to_string());
                        slot.notify.notify_waiters();
                    }
                }
            });
        }
        Ok(ServerStatus::Starting)
    }

    /// Wait up to `timeout` for the server to reach a terminal state (`Ready`
    /// or `Failed`); `NotStarted` (not installed) is returned immediately
    /// without kicking off a spawn.
    pub async fn wait_ready(
        &self,
        spec_id: &str,
        root: PathBuf,
        timeout: Duration,
    ) -> anyhow::Result<ServerStatus> {
        let status = self.ensure(spec_id, root.clone()).await?;
        if matches!(status, ServerStatus::NotStarted | ServerStatus::Ready) {
            return Ok(status);
        }
        let slot = self.slot_for(spec_id, root);
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let _ = tokio::time::timeout_at(deadline, slot.notify.notified()).await;
            let st = slot.state.lock().await;
            match &*st {
                SlotState::Ready(_) => return Ok(ServerStatus::Ready),
                SlotState::Failed(_) => return Ok(ServerStatus::Failed),
                SlotState::Idle | SlotState::Starting => {
                    if tokio::time::Instant::now() >= deadline {
                        return Ok(ServerStatus::Starting);
                    }
                    // Notified but still Starting — keep waiting on the budget.
                }
            }
        }
    }

    /// Code-intel entry point: ensure + bounded wait, return the `Ready`
    /// client. If still indexing after `CODE_INTEL_WAIT`, returns an "retry
    /// shortly" error rather than blocking forever. Resets a `Failed` slot
    /// to `Idle` so a server that died mid-session re-spawns on retry.
    pub async fn client_for(&self, spec_id: &str, root: PathBuf) -> anyhow::Result<Arc<LspClient>> {
        let status = self
            .wait_ready(spec_id, root.clone(), CODE_INTEL_WAIT)
            .await?;
        let slot = self.slot_for(spec_id, root.clone());
        let (client, failure) = {
            let st = slot.state.lock().await;
            match &*st {
                SlotState::Ready(c) => (Some(c.clone()), None),
                SlotState::Failed(reason) => (None, Some(reason.clone())),
                SlotState::Idle | SlotState::Starting => (None, None),
            }
        };
        match (status, client, failure) {
            (ServerStatus::Ready, Some(c), _) if c.is_ready() => Ok(c),
            // Cached client but the server died after Ready — reset and tell
            // the caller to retry, so the next call re-spawns.
            (ServerStatus::Ready, Some(_), _) => {
                let mut st = slot.state.lock().await;
                *st = SlotState::Idle;
                Err(anyhow!(
                    "LSP server `{spec_id}` exited unexpectedly; retry the call to re-spawn"
                ))
            }
            (ServerStatus::NotStarted, _, _) => Err(anyhow!(
                "LSP server `{spec_id}` is not on PATH; install it to enable code-intel"
            )),
            (ServerStatus::Starting, _, _) => Err(anyhow!(
                "LSP server `{spec_id}` still indexing; call LspWaitReady or retry shortly"
            )),
            (ServerStatus::Failed, _, Some(reason)) => {
                Err(anyhow!("LSP server `{spec_id}` failed to start: {reason}"))
            }
            (ServerStatus::Failed, _, None) => {
                Err(anyhow!("LSP server `{spec_id}` failed to start"))
            }
            (ServerStatus::Ready, None, _) => Err(anyhow!(
                "LSP server `{spec_id}` not in a usable state; retry the call"
            )),
        }
    }

    /// Snapshot every available server's status for a given root, for
    /// `LspStatus`. `Idle` (never spawned) surfaces as `NotStarted`.
    pub async fn statuses_for(&self, root: &Path) -> Vec<ServerReport> {
        let mut out = Vec::with_capacity(SPECS.len());
        for spec in SPECS {
            match self.availability(spec.id) {
                Some(ServerAvailability::NotInstalled) | None => {
                    out.push(ServerReport {
                        id: spec.id,
                        status: ServerStatus::NotStarted,
                        detail: Some("not installed".to_string()),
                    });
                    continue;
                }
                Some(ServerAvailability::Broken(reason)) => {
                    out.push(ServerReport {
                        id: spec.id,
                        status: ServerStatus::Failed,
                        detail: Some(reason.clone()),
                    });
                    continue;
                }
                Some(ServerAvailability::Available) => {}
            }
            let key = (spec.id.to_string(), root.to_path_buf());
            // Take the slot clone out of the cells lock before awaiting the
            // per-slot state lock — the std `Mutex` guard is not await-safe.
            let slot = self
                .cells
                .lock()
                .expect("cells mutex poisoned")
                .get(&key)
                .cloned();
            let (status, detail) = match slot {
                None => (ServerStatus::NotStarted, None),
                Some(slot) => {
                    let st = slot.state.lock().await;
                    match &*st {
                        SlotState::Idle => (ServerStatus::NotStarted, None),
                        SlotState::Starting => (ServerStatus::Starting, None),
                        SlotState::Ready(_) => (ServerStatus::Ready, None),
                        SlotState::Failed(reason) => (ServerStatus::Failed, Some(reason.clone())),
                    }
                }
            };
            out.push(ServerReport {
                id: spec.id,
                status,
                detail,
            });
        }
        out
    }
}

/// Probe `PATH` for every spec's server binary. Cheap (a few `which`
/// subprocesses); safe to run on the gpui main thread at startup. Never spawns
/// a server.
pub fn init() {
    let mut available = Vec::new();
    let mut availability = HashMap::new();
    for spec in SPECS {
        let state = probe_server(spec);
        if state == ServerAvailability::Available {
            available.push(spec);
        }
        availability.insert(spec.id, state);
    }
    let names: Vec<&str> = available.iter().map(|s| s.id).collect();
    if names.is_empty() {
        tracing::info!("LSP registry empty (no language servers on PATH)");
    } else {
        tracing::info!("LSP servers detected on PATH: {}", names.join(", "));
    }
    let registry = LspRegistry {
        available,
        availability,
        cells: SlotMap::default(),
    };
    if let Err(rejected) = REGISTRY.set(registry) {
        warn!(
            "LSP registry already initialized; new registry ({} available) rejected",
            rejected.available.len()
        );
    }
}

/// Run the spec's `detect` command; `which` exits 0 with the binary path on
/// stdout when found. macOS/Linux only (manox is darwin); `which` is universal
/// there.
fn binary_on_path(spec: &LspServerSpec) -> bool {
    let out = match std::process::Command::new(spec.detect[0])
        .args(&spec.detect[1..])
        .output()
    {
        Ok(o) => o,
        Err(_) => return false,
    };
    out.status.success() && !out.stdout.is_empty()
}

fn probe_server(spec: &LspServerSpec) -> ServerAvailability {
    if !binary_on_path(spec) {
        return ServerAvailability::NotInstalled;
    }
    let mut child = match std::process::Command::new(spec.probe[0])
        .args(&spec.probe[1..])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => return ServerAvailability::Broken(error.to_string()),
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return ServerAvailability::Broken("viability probe timed out after 2s".into());
            }
            Err(error) => return ServerAvailability::Broken(error.to_string()),
        }
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        let _ = pipe.read_to_string(&mut stdout);
    }
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    if status.success() {
        return ServerAvailability::Available;
    }
    let stderr = stderr.trim().to_string();
    let stdout = stdout.trim().to_string();
    let detail = if !stderr.is_empty() {
        stderr
    } else if !stdout.is_empty() {
        stdout
    } else {
        format!("probe exited with {status}")
    };
    ServerAvailability::Broken(detail.chars().take(500).collect())
}

pub fn global() -> &'static LspRegistry {
    REGISTRY
        .get()
        .expect("LspRegistry not initialized; call agent::init first")
}

pub fn try_global() -> Option<&'static LspRegistry> {
    REGISTRY.get()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_available() -> HashMap<&'static str, ServerAvailability> {
        SPECS
            .iter()
            .map(|spec| (spec.id, ServerAvailability::Available))
            .collect()
    }

    #[test]
    fn spec_for_path_matches_extension() {
        let reg = LspRegistry {
            available: SPECS.iter().collect(),
            availability: all_available(),
            cells: SlotMap::default(),
        };
        assert_eq!(
            reg.spec_for_path(Path::new("src/lib.rs")).unwrap().id,
            "rust-analyzer"
        );
        assert_eq!(
            reg.spec_for_path(Path::new("a/main.go")).unwrap().id,
            "gopls"
        );
        assert!(reg.spec_for_path(Path::new("README.md")).is_none());
    }

    #[test]
    fn is_available_respects_detected_set() {
        let reg = LspRegistry {
            available: vec![&SPECS[0]],
            availability: all_available(),
            cells: SlotMap::default(),
        };
        assert!(reg.is_available("rust-analyzer"));
        assert!(!reg.is_available("gopls"));
    }

    #[test]
    fn probe_rejects_a_resolvable_but_broken_executable() {
        static BROKEN: LspServerSpec = LspServerSpec {
            id: "broken-test",
            detect: &["which", "sh"],
            probe: &["sh", "-c", "echo probe-broken >&2; exit 7"],
            spawn: &["sh"],
            language_id: "test",
            extensions: &["test"],
            root_hints: &["test.root"],
        };
        let availability = probe_server(&BROKEN);
        assert!(
            matches!(availability, ServerAvailability::Broken(ref detail) if detail.contains("probe-broken")),
            "availability: {availability:?}"
        );
    }
}
