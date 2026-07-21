//! Process-global LSP registry: PATH detection at startup, lazy per-`(spec,
//! root)` client lifecycle.
//!
//! Mirrors `agent::mcp::registry`'s `OnceLock` shape. `init()` only probes
//! `PATH` — it never spawns. Per-`(spec_id, root)` slot, `ensure` kicks off a
//! detached spawn+initialize (non-blocking — returns `Starting` immediately),
//! `wait_ready` blocks on a `Notify` with a caller-chosen timeout, and
//! `client_for` is the convenience path code-intel tools use (ensure +
//! bounded wait, returns the `Ready` client or an "indexing, retry" error).
//!
//! The `AgentTool` adapters that surface this as tools live in the `agent`
//! crate (`agent::lsp`) to avoid a dependency cycle (`agent` depends on
//! `lsp`; `lsp` must not depend on `agent`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::{Mutex as StdMutex, OnceLock};
use std::time::Duration;

use anyhow::anyhow;
use tokio::sync::{Mutex, Notify};
use tracing::warn;

use crate::client::{LspClient, ServerStatus};
use crate::spec::{LspServerSpec, SPECS, spec_for_id};

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
    /// Specs whose server binary was found on `PATH` at `init` time. Only these
    /// are ever spawned; the rest degrade silently to grep/glob.
    available: Vec<&'static LspServerSpec>,
    cells: SlotMap,
}

impl LspRegistry {
    pub fn available_specs(&self) -> &[&'static LspServerSpec] {
        &self.available
    }

    pub fn is_available(&self, spec_id: &str) -> bool {
        self.available.iter().any(|s| s.id == spec_id)
    }

    /// Look up the spec for a file path's extension, among available servers.
    pub fn spec_for_path(&self, path: &Path) -> Option<&'static LspServerSpec> {
        let ext = path.extension()?.to_str()?;
        self.available
            .iter()
            .copied()
            .find(|s| s.extensions.contains(&ext))
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
        let client = {
            let st = slot.state.lock().await;
            match &*st {
                SlotState::Ready(c) => Some(c.clone()),
                _ => None,
            }
        };
        match (status, client) {
            (ServerStatus::Ready, Some(c)) if c.is_ready() => Ok(c),
            // Cached client but the server died after Ready — reset and tell
            // the caller to retry, so the next call re-spawns.
            (ServerStatus::Ready, Some(_)) => {
                let mut st = slot.state.lock().await;
                *st = SlotState::Idle;
                Err(anyhow!(
                    "LSP server `{spec_id}` exited unexpectedly; retry the call to re-spawn"
                ))
            }
            (ServerStatus::NotStarted, _) => Err(anyhow!(
                "LSP server `{spec_id}` is not on PATH; install it to enable code-intel"
            )),
            (ServerStatus::Starting, _) => Err(anyhow!(
                "LSP server `{spec_id}` still indexing; call LspWaitReady or retry shortly"
            )),
            (ServerStatus::Failed, _) => Err(anyhow!(
                "LSP server `{spec_id}` failed to start; see logs (reinstall or check PATH)"
            )),
            (ServerStatus::Ready, None) => Err(anyhow!(
                "LSP server `{spec_id}` not in a usable state; retry the call"
            )),
        }
    }

    /// Snapshot every available server's status for a given root, for
    /// `LspStatus`. `Idle` (never spawned) surfaces as `NotStarted`.
    pub async fn statuses_for(&self, root: &Path) -> Vec<(&'static str, ServerStatus)> {
        let mut out = Vec::with_capacity(self.available.len());
        for spec in &self.available {
            let key = (spec.id.to_string(), root.to_path_buf());
            // Take the slot clone out of the cells lock before awaiting the
            // per-slot state lock — the std `Mutex` guard is not await-safe.
            let slot = self
                .cells
                .lock()
                .expect("cells mutex poisoned")
                .get(&key)
                .cloned();
            let status = match slot {
                None => ServerStatus::NotStarted,
                Some(slot) => {
                    let st = slot.state.lock().await;
                    match &*st {
                        SlotState::Idle => ServerStatus::NotStarted,
                        SlotState::Starting => ServerStatus::Starting,
                        SlotState::Ready(_) => ServerStatus::Ready,
                        SlotState::Failed(_) => ServerStatus::Failed,
                    }
                }
            };
            out.push((spec.id, status));
        }
        out
    }
}

/// Probe `PATH` for every spec's server binary. Cheap (a few `which`
/// subprocesses); safe to run on the gpui main thread at startup. Never spawns
/// a server.
pub fn init() {
    let available = SPECS
        .iter()
        .filter(|s| binary_on_path(s))
        .collect::<Vec<_>>();
    let names: Vec<&str> = available.iter().map(|s| s.id).collect();
    if names.is_empty() {
        tracing::info!("LSP registry empty (no language servers on PATH)");
    } else {
        tracing::info!("LSP servers detected on PATH: {}", names.join(", "));
    }
    let registry = LspRegistry {
        available,
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

    #[test]
    fn spec_for_path_matches_extension() {
        let reg = LspRegistry {
            available: SPECS.iter().collect(),
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
            cells: SlotMap::default(),
        };
        assert!(reg.is_available("rust-analyzer"));
        assert!(!reg.is_available("gopls"));
    }
}
