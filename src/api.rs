//! Programmatic agent launch API: build an agent spec and drive a live PTY
//! session from process — inject input, read raw output, learn the injection
//! socket path, resize, and reap the child. The CLI is a thin driver over this.
//!
//! `SessionHandle` owns one PTY session's handles without touching the caller's
//! terminal (no raw mode, no `process::exit`), so a library caller (e.g. a future
//! `manox`) can start an agent, feed it messages, and consume its bytes directly.
//! `relay::run` wraps a handle to provide the interactive CLI behaviour.

use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};
use portable_pty::{ChildKiller, ExitStatus as PtyExitStatus};

use crate::relay::ipc::IpcServer;
use crate::relay::pty::{PtySession, spawn_pty};
use crate::relay::transfer::{WriteReq, writer_loop};
use crate::session::{self, SessionRegistry};
use crate::warp::WarpSession;
use crate::{
    LaunchSpec, ResolvedModel, Selection, WireApi, apply_probe_cache, build_all_models,
    build_launch_spec, find_agent, load_config, providers_for_agent, resolved_model_supports_agent,
};

/// The agent binary to launch. Codex.app is intentionally absent: it is a GUI
/// detach that does not fit the PTY/handle model this API exposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agent {
    Claude,
    Codex,
    Copilot,
    CodexPlus,
}

impl Agent {
    /// The canonical cx agent id matching `find_agent` / `resolved_agents`.
    fn id(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
            Agent::Copilot => "copilot",
            Agent::CodexPlus => "codex+",
        }
    }
}

/// Builder for an agent launch. Mirrors the CLI flags (`--pty`, `--socket`, the
/// agent name, provider/model selection, passthrough args) as a fluent API.
///
/// `provider` (and `model` when the provider has endpoints) are required: a
/// library caller has no terminal to pick interactively, so the selection must
/// be explicit. `spawn()` resolves the config, builds a `LaunchSpec`, and hands
/// it to `SessionHandle::spawn`.
pub struct AgentBuilder {
    pty: bool,
    socket: Option<PathBuf>,
    agent: Option<Agent>,
    provider: Option<String>,
    model: Option<String>,
    passthrough: Vec<String>,
    cwd: Option<PathBuf>,
}

impl Default for AgentBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentBuilder {
    pub fn new() -> Self {
        Self {
            pty: false,
            socket: None,
            agent: None,
            provider: None,
            model: None,
            passthrough: Vec::new(),
            cwd: None,
        }
    }

    /// Enable PTY relay (cx owns the master, terminal IO is transparent). When
    /// off, the child inherits stdio directly. Library callers usually want this
    /// on so `read`/`write`/`socket_path` are meaningful.
    pub fn pty(mut self, on: bool) -> Self {
        self.pty = on;
        self
    }

    /// Override the IPC injection socket path (`--socket`). Only effective with
    /// `pty(true)`. When unset, `spawn` uses `~/.config/cx/sessions/<id>.sock`.
    pub fn socket(mut self, path: impl Into<PathBuf>) -> Self {
        self.socket = Some(path.into());
        self
    }

    pub fn agent(mut self, agent: Agent) -> Self {
        self.agent = Some(agent);
        self
    }

    pub fn provider(mut self, name: impl Into<String>) -> Self {
        self.provider = Some(name.into());
        self
    }

    pub fn model(mut self, id: impl Into<String>) -> Self {
        self.model = Some(id.into());
        self
    }

    /// Working directory the spawned agent process runs in. When unset, the
    /// agent inherits cx's own cwd (the historical default). The override
    /// applies on both the PTY-relay and direct `Command::status()` spawn
    /// paths, so an embedding app can bind an external CLI session to a
    /// project directory without chdir'ing its own process.
    pub fn cwd(mut self, path: impl Into<PathBuf>) -> Self {
        self.cwd = Some(path.into());
        self
    }

    /// Extra args forwarded to the agent binary after cx's own (`--`-split).
    pub fn passthrough<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.passthrough.extend(args.into_iter().map(Into::into));
        self
    }

    /// Resolve config + selection and spawn the agent behind a `SessionHandle`.
    pub fn spawn(self) -> Result<SessionHandle> {
        let config = load_config()?;
        let mut all_models = build_all_models(&config);
        apply_probe_cache(&mut all_models);

        let agent_name = self
            .agent
            .ok_or_else(|| anyhow!("agent not set; call .agent(...) before .spawn()"))?
            .id();
        let agent =
            find_agent(&config, agent_name).ok_or_else(|| anyhow!("未知 agent: {agent_name}"))?;
        if agent.id == "Codex.app" {
            bail!("Codex.app 不支持 library API（GUI detach，无 PTY 句柄）");
        }

        let provider_name = self
            .provider
            .as_deref()
            .ok_or_else(|| anyhow!("library API 需显式指定 provider（无交互式选择）"))?;
        let provider = providers_for_agent(&config, &agent.id)
            .into_iter()
            .find(|p| p.name == provider_name)
            .with_context(|| {
                format!("agent `{}` 下未找到 provider `{}`", agent.id, provider_name)
            })?;

        let model = if provider.has_endpoints {
            let model_id = self
                .model
                .as_deref()
                .ok_or_else(|| anyhow!("provider `{}` 需要 model，但未指定", provider.name))?;
            let model: ResolvedModel = all_models
                .iter()
                .find(|m| {
                    m.provider_name == provider.name
                        && resolved_model_supports_agent(m, &agent.id)
                        && m.id == model_id
                })
                .with_context(|| {
                    format!(
                        "provider `{}` 下未找到支持 `{}` 的 model `{}`",
                        provider.name, agent.id, model_id
                    )
                })?
                .clone();
            Some(model)
        } else {
            None
        };

        // Pick a wire api the model offers that the agent supports; fall back to
        // the model's primary wire api, then the agent's first supported one.
        let selected_wire_api = model
            .as_ref()
            .and_then(|m| {
                m.model_wire_apis
                    .iter()
                    .find(|w| agent.supported_wire_apis.contains(w))
                    .copied()
            })
            .or_else(|| model.as_ref().map(|m| m.wire_api))
            .unwrap_or_else(|| {
                agent
                    .supported_wire_apis
                    .first()
                    .copied()
                    .unwrap_or(WireApi::Anthropic)
            });

        let selection = Selection {
            agent_id: agent.id.clone(),
            agent_binary: agent.binary.clone(),
            agent_args: agent.args.clone(),
            agent_env: agent.env.clone(),
            selected_wire_api,
            provider,
            model,
            injected_models: Vec::new(),
        };

        let socket = self
            .socket
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned());
        let spec = build_launch_spec(&selection, &self.passthrough, self.pty, socket, self.cwd)?;

        let warp_session =
            crate::warp::maybe_emit_session_start(&spec.agent_id, spec.model_id.as_deref());
        SessionHandle::spawn(&spec, warp_session)
    }
}

/// Outcome of a finished session: the agent's exit code, any termination note,
/// wall-clock duration, and the identifying triple for stats/telemetry.
#[derive(Debug, Clone)]
pub struct SessionResult {
    pub exit_code: i32,
    pub termination: Option<String>,
    pub duration: std::time::Duration,
    pub agent_id: String,
    pub provider_name: String,
    pub model_id: Option<String>,
}

/// Ownership of one live PTY agent session.
///
/// The master end lives in the background writer thread (it is `Send` but not
/// `Sync`, so it cannot be shared); `resize` therefore sends a request on the
/// writer channel rather than touching the master directly. The child, reader,
/// writer channel, injection socket, and Warp/warp stop bookkeeping all live
/// here so `wait` / `Drop` can finalize them without `process::exit`.
///
/// `child` and `finalized` are guarded so `wait`/`kill_wait` take `&self`:
/// a library caller can hold the handle behind `Arc` and still read, write,
/// resize, and reap from shared references.
pub struct SessionHandle {
    child: Mutex<Option<Box<dyn portable_pty::Child + Send>>>,
    /// Independent signal sender cloned from the child at spawn, so a library
    /// caller can `kill` the agent without racing the waiter thread's
    /// `child.lock().take()` reap. `ChildKiller::kill` is `&mut self`, hence
    /// the `Mutex` (a caller holds the handle behind `Arc`).
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    reader: Mutex<Box<dyn Read + Send>>,
    writer_tx: Mutex<mpsc::Sender<WriteReq>>,
    resize_flag: Arc<AtomicBool>,
    accept_shutdown: Arc<AtomicBool>,
    session_id: String,
    socket_path: PathBuf,
    /// Whether the injection socket was actually bound. False when `IpcServer::bind`
    /// failed, in which case `socket_path` is the intended-but-dead path and
    /// `socket_path()` returns `None` so callers don't try to connect to it.
    socket_bound: bool,
    agent_id: String,
    provider_name: String,
    model_id: Option<String>,
    warp_session: Option<WarpSession>,
    started_at: Instant,
    finalized: AtomicBool,
}

impl SessionHandle {
    /// Spawn the agent per `spec`, bind the injection socket, write the session
    /// registry, and start the writer + IPC acceptor threads. Does NOT enter raw
    /// mode, forward stdin, or print anything — those are the CLI driver's call.
    ///
    /// `warp_session` is taken by value: the handle owns the paired stop event.
    pub(crate) fn spawn(spec: &LaunchSpec, warp_session: Option<WarpSession>) -> Result<Self> {
        let session_id = session::generate_session_id();
        let started_at = Instant::now();

        let pty = spawn_pty(spec, &crate::relay::warp_env(&warp_session))?;
        let PtySession {
            master,
            child,
            reader,
            writer,
        } = pty;

        // Clone the killer before `child` moves into its Mutex, so a library
        // caller can signal kill through `&self` without touching the child.
        let killer = child.clone_killer();

        let (tx, rx) = mpsc::channel::<WriteReq>();

        // Resolve the socket path: a `--socket` override wins, else the default
        // per-session path. The registry advertises whichever is bound.
        let socket_path = match spec.socket.as_deref() {
            Some(p) => Ok(PathBuf::from(p)),
            None => session::socket_path(&session_id),
        }
        .context("解析 socket 路径失败")?;

        // IPC is best-effort: the session still works without external injection.
        let mut socket_bound = false;
        let accept_shutdown = Arc::new(AtomicBool::new(false));
        match IpcServer::bind(&socket_path) {
            Ok(ipc) => {
                ipc.accept_loop(tx.clone(), Arc::clone(&accept_shutdown));
                let reg = SessionRegistry {
                    id: session_id.clone(),
                    socket: socket_path.to_string_lossy().into_owned(),
                    pid: std::process::id(),
                    agent: spec.agent_id.clone(),
                    model: spec.model_id.clone(),
                    provider: spec.provider_name.clone(),
                    started_at: chrono::Utc::now().to_rfc3339(),
                    cwd: std::env::current_dir()
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                };
                let _ = session::write_registry(&reg);
                socket_bound = true;
            }
            Err(e) => eprintln!("cx: IPC 绑定失败（外部注入不可用）: {e}"),
        }

        let resize_flag = Arc::new(AtomicBool::new(false));
        writer_loop(writer, rx, master, resize_flag.clone());

        Ok(Self {
            child: Mutex::new(Some(child)),
            killer: Mutex::new(killer),
            reader: Mutex::new(reader),
            writer_tx: Mutex::new(tx),
            resize_flag,
            accept_shutdown,
            session_id,
            socket_path,
            socket_bound,
            agent_id: spec.agent_id.clone(),
            provider_name: spec.provider_name.clone(),
            model_id: spec.model_id.clone(),
            warp_session,
            started_at,
            finalized: AtomicBool::new(false),
        })
    }

    /// Read raw master bytes (ANSI/control sequences included). A `0` return
    /// means the agent closed its stdout, i.e. it is exiting.
    pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        let mut reader = self.reader.lock().expect("reader mutex poisoned");
        reader.read(buf)
    }

    /// Inject a line of text to the agent as if the user typed it and pressed
    /// enter (a trailing `'\n'` is appended, matching `cx send`). Returns an
    /// error only if the writer thread has exited (master closed).
    pub fn write(&self, text: &str) -> Result<()> {
        let mut bytes = text.as_bytes().to_vec();
        bytes.push(b'\n');
        self.writer_tx
            .lock()
            .expect("writer_tx mutex poisoned")
            .send(WriteReq::Bytes(bytes))
            .map_err(|_| anyhow!("agent 已退出，写入失败"))
    }

    /// Clear the agent's current input area. Sends Ctrl+U then enter; an empty
    /// submit is ignored by claude code, so the net effect is a cleared box.
    /// Equivalent to `cx send --clear-buffer` (no text).
    pub fn clear_buffer(&self) -> Result<()> {
        self.write(crate::send::CLEAR_INPUT)
    }

    /// Clear the input area, then overwrite it with `text` and submit. Equivalent
    /// to `cx send --clear-buffer <text>`.
    pub fn write_over(&self, text: &str) -> Result<()> {
        self.write(&format!("{}{text}", crate::send::CLEAR_INPUT))
    }

    /// Write raw bytes to the agent's PTY master verbatim — no trailing
    /// newline. A library host driving the agent's TUI keystroke-by-keystroke
    /// (arrow keys, Ctrl+C, paste) uses this; `write` is for line-based
    /// injection. Returns an error only if the writer thread has exited.
    pub fn write_bytes(&self, bytes: &[u8]) -> Result<()> {
        self.writer_tx
            .lock()
            .expect("writer_tx mutex poisoned")
            .send(WriteReq::Bytes(bytes.to_vec()))
            .map_err(|_| anyhow!("agent 已退出，写入失败"))
    }

    /// Path of the bound injection socket — `cx send` / external peers connect
    /// here, and a library caller may do the same out-of-process. Returns `None`
    /// when the IPC bind failed, so callers never advertise a dead socket.
    pub fn socket_path(&self) -> Option<&Path> {
        self.socket_bound.then_some(self.socket_path.as_path())
    }

    /// Resize the agent's PTY to an explicit size. Sent to the writer thread
    /// (which owns the master) rather than applied inline, since `MasterPty` is
    /// not `Sync`.
    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        self.writer_tx
            .lock()
            .expect("writer_tx mutex poisoned")
            .send(WriteReq::Resize(cols, rows))
            .map_err(|_| anyhow!("agent 已退出，resize 失败"))
    }

    /// The SIGWINCH flag the CLI's signal handler sets; the writer thread polls
    /// it to sync the master size to the user terminal. Library callers ignore.
    pub(crate) fn resize_flag(&self) -> Arc<AtomicBool> {
        self.resize_flag.clone()
    }

    pub(crate) fn writer_tx(&self) -> mpsc::Sender<WriteReq> {
        self.writer_tx
            .lock()
            .expect("writer_tx mutex poisoned")
            .clone()
    }

    /// Block until the agent exits naturally, then clean up (IPC socket +
    /// registry, Warp stop event) and return the outcome. Does not print or exit.
    ///
    /// Takes `&self` so a caller sharing the handle behind `Arc` can reap it
    /// from a background thread without giving up ownership. The child is taken
    /// out of its `Mutex` and reaped here; a second call returns
    /// `child already reaped`.
    pub fn wait(&self) -> Result<SessionResult> {
        let mut child = self
            .child
            .lock()
            .expect("child mutex poisoned")
            .take()
            .context("child already reaped")?;
        let status = child
            .wait()
            .unwrap_or_else(|_| PtyExitStatus::with_exit_code(1));
        Ok(self.finalize(&status))
    }

    /// Signal the agent to terminate (SIGKILL on Unix). Non-blocking: the waiter
    /// thread (if running `wait`) reaps the child; otherwise `wait`/`Drop` does.
    /// Safe to call from a thread that does not own the child, since it goes
    /// through the cloned `ChildKiller` rather than the `child` Mutex.
    pub fn kill(&self) -> io::Result<()> {
        self.killer.lock().expect("killer mutex poisoned").kill()
    }

    /// Kill the child, reap it, finalize, and return the outcome. Used by the
    /// CLI on the raw-mode-failure path where the agent must be torn down.
    pub(crate) fn kill_wait(&self) -> SessionResult {
        let status = match self.child.lock().expect("child mutex poisoned").take() {
            Some(mut child) => {
                let _ = child.kill();
                child
                    .wait()
                    .unwrap_or_else(|_| PtyExitStatus::with_exit_code(1))
            }
            None => PtyExitStatus::with_exit_code(1),
        };
        self.finalize(&status)
    }

    /// Compute exit code/termination, remove the IPC socket + registry, emit the
    /// Warp stop, and mark finalized so `Drop` is a no-op. Returns the result.
    fn finalize(&self, status: &PtyExitStatus) -> SessionResult {
        let exit_code = crate::relay::pty_exit_code(status);
        let termination = if status.success() && exit_code == 0 {
            None
        } else {
            Some(status.to_string())
        };

        // Stop the accept loop (non-blocking, polls this flag) so its thread and
        // sender clone don't outlive the session. Done before removing the socket
        // file so the loop exits even if a peer is mid-handshake.
        self.accept_shutdown.store(true, Ordering::Relaxed);

        // Remove the bound socket (may be a custom `--socket` path) and the
        // registry sibling so neither lingers as a stale session.
        let _ = std::fs::remove_file(&self.socket_path);
        if let Ok(reg_path) = session::registry_path(&self.session_id) {
            let _ = std::fs::remove_file(reg_path);
        }

        if let Some(ws) = &self.warp_session {
            ws.emit_stop(Some(exit_code));
        }

        self.finalized.store(true, Ordering::Relaxed);
        SessionResult {
            exit_code,
            termination,
            duration: self.started_at.elapsed(),
            agent_id: self.agent_id.clone(),
            provider_name: self.provider_name.clone(),
            model_id: self.model_id.clone(),
        }
    }
}

impl Drop for SessionHandle {
    fn drop(&mut self) {
        // Only when the caller dropped without `wait`/`kill_wait`: best-effort
        // kill + reap so the child is not orphaned, then remove session files.
        // The Warp stop is handled by `WarpSession`'s own Drop (exit_code None).
        if !self.finalized.load(Ordering::Relaxed) {
            self.accept_shutdown.store(true, Ordering::Relaxed);
            if let Some(mut child) = self.child.lock().expect("child mutex poisoned").take() {
                let _ = child.kill();
                let _ = child.wait();
            }
            let _ = std::fs::remove_file(&self.socket_path);
            if let Ok(reg_path) = session::registry_path(&self.session_id) {
                let _ = std::fs::remove_file(reg_path);
            }
        }
    }
}

/// Compile-time guarantee that a `SessionHandle` is shareable behind `Arc`
/// across threads — a host (manox) reads from a background thread, writes /
/// resizes from the UI thread, and reaps from a waiter thread.
const _: () = {
    fn _assert_send_sync<T: Send + Sync>() {}
    fn _f() {
        _assert_send_sync::<SessionHandle>();
    }
};

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    /// A `LaunchSpec` for `/bin/cat` — echoes input on the master reader, so a
    /// `write` is observable via `read`, and `wait` returns exit 0 on EOF.
    fn cat_spec(socket: Option<String>) -> LaunchSpec {
        LaunchSpec {
            program: PathBuf::from("/bin/cat"),
            args: vec![],
            env: BTreeMap::new(),
            summary: String::new(),
            detach: false,
            env_remove: vec![],
            agent_id: "test".into(),
            provider_name: "test".into(),
            model_id: None,
            pty: true,
            socket,
            cwd: None,
        }
    }

    /// Poll `handle.read` until `needle` appears in the accumulated output, or
    /// the deadline passes (returns whatever was collected either way).
    fn read_until_contains(handle: &SessionHandle, needle: &[u8], deadline: Instant) -> Vec<u8> {
        let mut buf = [0u8; 128];
        let mut out = Vec::new();
        while Instant::now() < deadline {
            if let Ok(n) = handle.read(&mut buf) {
                if n > 0 {
                    out.extend_from_slice(&buf[..n]);
                    if out.windows(needle.len()).any(|w| w == needle) {
                        break;
                    }
                }
            }
        }
        out
    }

    #[test]
    fn session_handle_round_trips_write_and_read() {
        let spec = cat_spec(None);
        let handle = SessionHandle::spawn(&spec, None).expect("spawn");

        handle.write("hello").expect("write");

        let deadline = Instant::now() + Duration::from_secs(2);
        let out = read_until_contains(&handle, b"hello", deadline);
        assert!(
            out.windows(5).any(|w| w == b"hello"),
            "expected cat to echo 'hello' via SessionHandle, got: {:?}",
            String::from_utf8_lossy(&out)
        );

        // socket_path is advertised (Some) and live (connect probe succeeds)
        let sock = handle.socket_path().expect("socket should be bound");
        assert!(session::socket_alive(sock));

        // `/bin/cat` never exits on its own (it waits for stdin EOF), so the
        // handle must be killed to reap it. kill_wait reaps + cleans up the IPC
        // socket/registry and returns a termination note (SIGKILL is not success).
        let res = handle.kill_wait();
        assert_ne!(res.exit_code, 0);
        assert!(res.termination.is_some());
    }

    #[test]
    fn session_handle_external_socket_injection_reaches_read() {
        // Use a custom socket path under a temp dir so the test is hermetic.
        let tmp = std::env::temp_dir().join(format!(
            "cx-api-inject-{}-{}.sock",
            std::process::id(),
            uuid_v4_simple()
        ));
        let spec = cat_spec(Some(tmp.to_string_lossy().into_owned()));
        let handle = SessionHandle::spawn(&spec, None).expect("spawn");

        // Wait for the server to be accepting, then inject from out-of-process.
        let mut connected = None;
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if let Ok(s) = UnixStream::connect(&tmp) {
                connected = Some(s);
                break;
            }
        }
        let mut peer = connected.expect("connect to injection socket");
        peer.write_all(b"{\"text\":\"injected\"}\n").expect("write");
        peer.flush().expect("flush");
        drop(peer);

        let deadline = Instant::now() + Duration::from_secs(2);
        let out = read_until_contains(&handle, b"injected", deadline);
        assert!(
            out.windows(8).any(|w| w == b"injected"),
            "expected external injection to surface on read, got: {:?}",
            String::from_utf8_lossy(&out)
        );

        // kill_wait reaps cat and removes the (custom) socket file + registry.
        let res = handle.kill_wait();
        assert_ne!(res.exit_code, 0);
        assert!(!tmp.exists(), "custom socket should be removed on reap");
    }

    #[test]
    fn session_handle_resize_does_not_crash() {
        let spec = cat_spec(None);
        let handle = SessionHandle::spawn(&spec, None).expect("spawn");
        // A resize request must be accepted by the writer thread without error
        // while the session is live.
        handle.resize(120, 40).expect("resize");
        let _ = handle.kill_wait();
    }

    #[test]
    fn agent_builder_missing_provider_errors() {
        // Without a real cx config this errors at config/selection, but a missing
        // provider is caught before any IO regardless of the environment.
        let r = AgentBuilder::new().agent(Agent::Claude).pty(true).spawn();
        assert!(r.is_err());
    }

    /// Simple UUIDv4-style hex suffix to keep per-test socket paths unique without
    /// pulling a uuid dependency into the test module.
    fn uuid_v4_simple() -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut h = DefaultHasher::new();
        Instant::now().hash(&mut h);
        std::thread::current().id().hash(&mut h);
        format!("{:016x}", h.finish())
    }
}
