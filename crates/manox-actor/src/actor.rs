//! Agent actor thread.
//!
//! Hosts the gpui `HeadlessAppContext`, the command channel, and the
//! shutdown sentinel around the session-orchestration core
//! (`manox-session-core`). Commands arrive as JSON strings; the loop parses
//! them, handles the host-local `init` (agent boot + host identity + ready
//! handshake), and delegates every session command to the core's
//! `handle_command` on the context's `App`. The foreground executor is
//! driven with `run_until_parked` while any session's turn is active, and
//! the thread waits on the command channel when idle, waking periodically
//! so parked async work still lands.

use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use gpui::HeadlessAppContext;
use serde_json::Value;

pub use manox_session_core::session::EventSink;

/// Sentinel command terminating the actor thread; see `ActorHandle::shutdown`.
const SHUTDOWN: &str = "__shutdown__";

/// Host-side handle to the actor thread.
pub struct ActorHandle {
    tx: mpsc::Sender<String>,
    _thread: thread::JoinHandle<()>,
}

impl ActorHandle {
    pub fn send(&self, command: String) -> Result<(), String> {
        self.tx.send(command).map_err(|e| e.to_string())
    }

    /// Ask the actor thread to drain and exit, releasing the event sink.
    /// Returns without waiting for the thread so host teardown never blocks
    /// on in-flight work; the detached thread exits on the sentinel.
    pub fn shutdown(self) {
        let _ = self.tx.send(SHUTDOWN.to_string());
    }
}

/// Spawn the actor thread; returns a handle for command delivery.
pub fn start(sink: EventSink) -> Result<ActorHandle, std::io::Error> {
    let (tx, rx) = mpsc::channel::<String>();
    let handle = thread::Builder::new()
        .name("manox-agent".into())
        .spawn(move || run_actor(rx, sink))?;
    Ok(ActorHandle {
        tx,
        _thread: handle,
    })
}

fn run_actor(rx: mpsc::Receiver<String>, sink: EventSink) {
    let mut cx = HeadlessAppContext::new(Arc::new(gpui::NoopTextSystem));
    cx.allow_parking();
    run_command_loop(&mut cx, rx, &sink);
    // `init` registered the ThreadStore entity in a process-global slot;
    // release it before the context drops so gpui's leaked-handle check
    // stays quiet across shutdown / window-reload cycles.
    agent::thread_store::drop_global_for_test();
}

fn run_command_loop(cx: &mut HeadlessAppContext, rx: mpsc::Receiver<String>, sink: &EventSink) {
    let mut state = manox_session_core::session::ActorState::new(
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
    );

    loop {
        let mut had_command = false;
        while let Ok(cmd) = rx.try_recv() {
            had_command = true;
            if !handle_command(cx, &mut state, sink, &cmd) {
                return;
            }
        }
        // Drive the foreground executor so pending async work (streaming,
        // tool callbacks) progresses and events are emitted.
        cx.run_until_parked();
        if had_command || state.any_turn_active() {
            // A turn is in flight; keep driving without blocking the channel.
            thread::sleep(Duration::from_millis(5));
            continue;
        }
        // Idle: wait for the next command, but wake periodically so parked
        // async work (the thread-directory scan behind list_threads and its
        // follow-up snapshot push) still lands — an unconditional recv()
        // would freeze those events until a command happens to arrive.
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(cmd) => {
                if !handle_command(cx, &mut state, sink, &cmd) {
                    return;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// Parse and route one command. Returns `false` only for the shutdown
/// sentinel. The host-local `init` boots the agent and pins the declaring
/// host identity; every session command delegates to the orchestration core.
fn handle_command(
    cx: &mut HeadlessAppContext,
    state: &mut manox_session_core::session::ActorState,
    sink: &EventSink,
    command: &str,
) -> bool {
    if command == SHUTDOWN {
        return false;
    }
    let cmd: Value = match serde_json::from_str(command) {
        Ok(v) => v,
        Err(_) => return true,
    };
    let Some(cmd_name) = cmd["cmd"].as_str() else {
        return true;
    };
    if cmd_name != "init" {
        return cx
            .update(|app| manox_session_core::session::handle_command(app, state, sink, &cmd));
    }
    if let Some(cwd) = cmd["cwd"].as_str() {
        state.cwd = PathBuf::from(cwd);
    }
    // The declaring host pins its identity before `agent::init` computes
    // host-scoped session state.
    if let Some(host) = cmd["host"].as_str().and_then(agent::host::Host::from_slug) {
        agent::host::set_host(host);
    } else if cmd.get("host").is_some() {
        eprintln!(
            "manox actor: unrecognized host slug on init: {:?}",
            cmd["host"]
        );
    }
    cx.update(agent::init);
    sink.emit(r#"{"type":"ready"}"#.to_string());
    manox_session_core::session::spawn_models_push(sink.clone());
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::{Mutex, Once};
    use std::time::Duration;

    use manox_session_core::session::ActorState;

    /// Session-creating tests mutate `HOME` and initialize `OnceLock`
    /// globals, so they must not interleave with each other.
    static GLOBALS_LOCK: Mutex<()> = Mutex::new(());
    static HOME_ONCE: Once = Once::new();
    static INIT_ONCE: Once = Once::new();

    /// Point `HOME` at a throwaway directory so the thread db and provider
    /// config lookups stay out of the developer's real config. Never
    /// restored: the test process is disposable and provider registration
    /// reads `HOME` from a background thread.
    fn hermetic_home() {
        HOME_ONCE.call_once(|| {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let home = std::env::temp_dir()
                .join(format!("manox-actor-test-{}-{nanos}", std::process::id()));
            std::fs::create_dir_all(&home).unwrap();
            // SAFETY: test setup, serialized behind GLOBALS_LOCK.
            unsafe { std::env::set_var("HOME", home) };
        });
    }

    /// The tokio runtime and provider registry are process-wide `OnceLock`
    /// globals; initialize them exactly once, lightweight variants only
    /// (`agent::init` would also boot MCP/LSP/plugin subsystems).
    fn init_globals(_cx: &mut HeadlessAppContext) {
        INIT_ONCE.call_once(|| {
            agent::runtime::init();
            agent::pi_providers::init();
        });
    }

    fn collect_sink() -> (Arc<Mutex<Vec<String>>>, EventSink) {
        let out = Arc::new(Mutex::new(Vec::new()));
        let sink_out = out.clone();
        (
            out.clone(),
            EventSink::new(move |json| sink_out.lock().unwrap().push(json)),
        )
    }

    fn types(out: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
        out.lock()
            .unwrap()
            .iter()
            .map(|raw| {
                serde_json::from_str::<serde_json::Value>(raw).unwrap()["type"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn shutdown_sentinel_ends_the_loop() {
        let mut cx = HeadlessAppContext::new(Arc::new(gpui::NoopTextSystem));
        cx.allow_parking();
        let mut state = ActorState::new(PathBuf::from("/"));
        let (out, sink) = collect_sink();
        // A command for an unknown session is handled (error event) and
        // keeps the loop alive; only the sentinel terminates it.
        assert!(handle_command(
            &mut cx,
            &mut state,
            &sink,
            r#"{"cmd":"get_usage","sessionId":"ghost"}"#
        ));
        assert!(types(&out).contains(&"error".to_string()));
        assert!(!handle_command(&mut cx, &mut state, &sink, SHUTDOWN));
    }

    #[test]
    fn idle_loop_drives_the_async_thread_scan() {
        let _guard = GLOBALS_LOCK.lock().unwrap();
        hermetic_home();
        // A session population large enough that the directory scan outlives
        // the command-processing pumps; its follow-up snapshot then lands
        // only if the idle loop keeps driving the executor. A distinct cwd
        // keeps other tests' project-scoped snapshots empty.
        let sessions = agent::paths::manox_config_dir()
            .expect("config dir")
            .join("pi-sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        for i in 0..1000 {
            std::fs::write(
                sessions.join(format!("idle-scan-{i}.jsonl")),
                format!(
                    "{{\"type\":\"session\",\"version\":3,\"id\":\"idle-scan-{i}\",\"timestamp\":\"2026-05-28T07:13:46.608Z\",\"cwd\":\"/idle/pump/project\"}}\n"
                ),
            )
            .unwrap();
        }
        let (out, sink) = collect_sink();
        let (tx, rx) = mpsc::channel::<String>();
        let actor = thread::spawn(move || {
            let mut cx = HeadlessAppContext::new(Arc::new(gpui::NoopTextSystem));
            cx.allow_parking();
            init_globals(&mut cx);
            cx.update(agent::thread_store::init);
            run_command_loop(&mut cx, rx, &sink);
            agent::thread_store::drop_global_for_test();
        });
        tx.send(r#"{"cmd":"list_threads"}"#.to_string()).unwrap();
        // The immediate snapshot precedes the scan; the follow-up push only
        // lands if the idle loop keeps pumping instead of blocking on recv().
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let pushed = types(&out)
                .iter()
                .filter(|t| t.as_str() == "threads_updated")
                .count();
            if pushed >= 2 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "follow-up threads_updated never arrived while the loop sat idle"
            );
            thread::sleep(Duration::from_millis(10));
        }
        drop(tx); // disconnect ends the loop
        actor.join().unwrap();
    }

    /// Session files the store scan picks up: one header line per session.
    fn seed_session_file(dir: &Path, id: &str, cwd: &str) {
        std::fs::write(
            dir.join(format!("{id}.jsonl")),
            format!(
                "{{\"type\":\"session\",\"version\":3,\"id\":\"{id}\",\"timestamp\":\"2026-05-28T07:13:46.608Z\",\"cwd\":\"{cwd}\"}}\n"
            ),
        )
        .unwrap();
    }

    /// Seed a session file with an explicit header `metadata.host` tag.
    fn seed_session_file_meta(dir: &Path, id: &str, cwd: &str, host: &str) {
        std::fs::write(
            dir.join(format!("{id}.jsonl")),
            format!(
                "{{\"type\":\"session\",\"version\":3,\"id\":\"{id}\",\"timestamp\":\"2026-05-28T07:13:46.608Z\",\"cwd\":\"{cwd}\",\"metadata\":{{\"host\":\"{host}\"}}}}\n"
            ),
        )
        .unwrap();
    }

    /// Restore the process host identity on drop so a panicking test cannot
    /// leak a switched host into later tests.
    struct HostGuard(agent::host::Host);
    impl Drop for HostGuard {
        fn drop(&mut self) {
            agent::host::set_host(self.0);
        }
    }

    #[test]
    fn init_command_scopes_threads_to_the_declared_host() {
        let _guard = GLOBALS_LOCK.lock().unwrap();
        hermetic_home();
        let sessions = agent::paths::manox_config_dir()
            .expect("config dir")
            .join("pi-sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        seed_session_file(&sessions, "legacy", "/iso/project");
        seed_session_file_meta(&sessions, "vscode-one", "/iso/project", "vscode");

        let mut cx = HeadlessAppContext::new(Arc::new(gpui::NoopTextSystem));
        cx.allow_parking();
        init_globals(&mut cx);
        let mut state = ActorState::new(PathBuf::from("/iso/project"));
        let (out, sink) = collect_sink();
        let snapshot_ids =
            |out: &Arc<Mutex<Vec<String>>>, cx: &mut HeadlessAppContext| -> Vec<String> {
                cx.run_until_parked();
                out.lock()
                    .unwrap()
                    .iter()
                    .rev()
                    .map(|raw| serde_json::from_str::<Value>(raw).unwrap())
                    .find(|e| e["type"] == "threads_updated")
                    .and_then(|s| s["threads"].as_array().cloned())
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|t| t["id"].as_str().map(str::to_string))
                    .collect()
            };
        let wait_for =
            |out: &Arc<Mutex<Vec<String>>>, cx: &mut HeadlessAppContext, expected: &[&str]| {
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                loop {
                    let ids = snapshot_ids(out, cx);
                    if ids.iter().map(String::as_str).eq(expected.iter().copied()) {
                        break;
                    }
                    assert!(
                        std::time::Instant::now() < deadline,
                        "snapshot never matched {expected:?}; last ids: {ids:?}"
                    );
                    thread::sleep(Duration::from_millis(10));
                }
            };

        // Default host (the native app): the untagged session is the only
        // one visible.
        cx.update(agent::thread_store::init);
        handle_command(&mut cx, &mut state, &sink, r#"{"cmd":"list_threads"}"#);
        wait_for(&out, &mut cx, &["legacy"]);
        agent::thread_store::drop_global_for_test();

        // A vscode host: only the tagged session is visible.
        let host_guard = HostGuard(agent::host::current());
        agent::host::set_host(agent::host::Host::Vscode);
        cx.update(agent::thread_store::init);
        // The store global was rebuilt; re-subscribe so follow-up snapshot
        // pushes land on the new entity.
        state.store_subscription = None;
        handle_command(&mut cx, &mut state, &sink, r#"{"cmd":"list_threads"}"#);
        wait_for(&out, &mut cx, &["vscode-one"]);
        drop(host_guard);

        drop(state);
        agent::thread_store::drop_global_for_test();
    }
}
