//! Live reproduction for the "execute plan in fresh context" handoff: after
//! an `ExecuteFresh` verdict the workspace spawns a fresh thread seeded with
//! the execution directive and runs its first turn. The sidebar renders the
//! `ThreadStore` summary list, which is refreshed on `refresh_thread_list` and
//! the actor's `SessionListDirty` notices. The pi session file for a fresh
//! session is deferred until the first assistant message, so a refresh that
//! scans before the file materializes misses the thread entirely.
//!
//! This test mirrors the workspace's ExecuteFresh orchestration at the
//! facade + store level (no UI): create the fresh thread, seed the execution
//! turn, wait for the turn to settle, then apply the workspace's
//! `TurnFinished` `refresh_thread_list` refresh and assert the new thread's
//! session surfaces in `ThreadStore::summaries` — the sidebar's list.
//!
//! Hermetic: the provider config points at a fake Anthropic endpoint served
//! in-process (plain TCP + SSE), so no external API is contacted.
//!
//! Run with:
//! ```sh
//! MANOX_RUN_LIVE=1 cargo test -p agent --test execute_fresh_sidebar -- --ignored --nocapture
//! ```

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use manox_agent::{Thread, ThreadId};

/// Serve a canned Anthropic SSE reply (a single assistant text block, stop
/// reason `end_turn`) on an ephemeral port. Returns the port.
fn spawn_fake_anthropic() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake anthropic");
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            std::thread::spawn(move || {
                serve_one(&mut stream);
            });
        }
    });
    port
}

fn serve_one(stream: &mut TcpStream) {
    // Drain the request (headers + body) — the reply is unconditional.
    let mut buf = [0u8; 8192];
    let mut filled = 0;
    loop {
        match stream.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => {
                filled += n;
                if filled == buf.len() {
                    break;
                }
                if buf[..filled].windows(4).any(|w| w == b"\r\n\r\n") && filled > 0 {
                    // Headers seen; the body (if any) is small, one more read
                    // picks it up. Keep draining until the stream would block.
                    if stream.read(&mut buf).is_err() {
                        break;
                    }
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let mut body = String::new();
    let mut evt = |json: &str| body.push_str(&format!("data: {json}\n\n"));
    evt(
        r#"{"type":"message_start","message":{"id":"msg_fake_1","model":"fake-model","role":"assistant","content":[],"usage":{"input_tokens":5,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}"#,
    );
    evt(r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#);
    evt(
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Plan executed."}}"#,
    );
    evt(r#"{"type":"content_block_stop","index":0}"#);
    evt(
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":null}"#,
    );
    evt(r#"{"type":"message_stop"}"#);
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{body}"
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

/// Restore `HOME` even when the test panics mid-way.
struct RestoreHome(Option<std::ffi::OsString>);
impl Drop for RestoreHome {
    fn drop(&mut self) {
        if let Some(home) = &self.0 {
            unsafe { std::env::set_var("HOME", home) };
        } else {
            unsafe { std::env::remove_var("HOME") };
        }
    }
}

#[test]
#[ignore = "requires MANOX_RUN_LIVE=1 (spawns a hermetic fake anthropic endpoint)"]
fn execute_fresh_spawned_thread_surfaces_in_store() {
    if std::env::var("MANOX_RUN_LIVE").is_err() {
        eprintln!("skipping: MANOX_RUN_LIVE not set");
        return;
    }

    let port = spawn_fake_anthropic();

    // Isolate config + sessions under a temp HOME.
    let home = std::env::temp_dir().join(format!("manox-execute-fresh-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(home.join(".manox")).unwrap();
    std::fs::create_dir_all(home.join(".manox/pi-sessions")).unwrap();
    std::fs::write(
        home.join(".manox/cx.providers.config.yaml"),
        format!(
            "providers:\n- name: Fake\n  apikey_source: literal:test-key\n  endpoints:\n    anthropic: http://127.0.0.1:{port}\n  models:\n    fake-model:\n      wire_apis: [anthropic]\n      context: 200000\n"
        ),
    )
    .unwrap();
    let old_home = std::env::var_os("HOME");
    unsafe { std::env::set_var("HOME", &home) };
    let _restore = RestoreHome(old_home);

    let project = home.join("project");
    std::fs::create_dir_all(&project).unwrap();

    let cx = gpui::TestAppContext::single();
    cx.update(|_| {
        manox_agent::runtime::init();
        manox_agent::settings::init_optimization();
        manox_agent::i18n::init();
        manox_agent::pi_providers::init();
        manox_agent::thread_store::init();
    });
    // Provider registration runs on a background thread; block until it lands
    // so `default_model` resolves before the thread is constructed.
    cx.update(|_| {
        manox_agent::runtime::handle().block_on(manox_agent::pi_providers::wait_ready());
    });

    // Mirror `respond_plan_review`'s ExecuteFresh arm: spawn a fresh
    // project-bound thread and seed the execution turn.
    let thread =
        Thread::new_in_project(ThreadId(uuid::Uuid::new_v4().to_string()), project.clone());
    let new_id = thread.read(|t| t.id.0.clone());
    thread.with_mut(|t| {
        t.seed_plan_execution(
            "/tmp/test-plan.md".to_string(),
            "Reply with just OK.".to_string(),
            None,
        );
    });

    // Wait for the seeded turn to settle (the actor streams through the fake
    // endpoint, materializing the deferred session file on the first
    // assistant message). The facade flips `running` false on `Settled`. The
    // engine pump runs on the tokio runtime, so poll `is_running` rather than
    // park the gpui context.
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let done = thread.read(|t| !t.is_running());
        if done || Instant::now() > deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let running = thread.read(|t| t.is_running());
    assert!(
        !running,
        "seeded execution turn did not settle within 120s (fake endpoint broken?)"
    );

    // The workspace's `TurnFinished` handler persists with `touch=true`,
    // which re-scans the session repository into the store summaries.
    // Apply the same call; the scan runs on the agent runtime.
    manox_agent::refresh_thread_list();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let listed =
            manox_agent::thread_store_global().read(|s| s.summaries().iter().any(|s| s.id == new_id));
        if listed {
            break;
        }
        if Instant::now() > deadline {
            // Debug dump: what does the session repository actually contain?
            let dir = manox_agent::paths::manox_config_dir()
                .expect("manox config dir")
                .join("pi-sessions");
            let mut files = Vec::new();
            if let Ok(rd) = std::fs::read_dir(&dir) {
                for entry in rd.flatten() {
                    files.push(entry.path().display().to_string());
                }
            }
            let listed = manox_agent::thread_store_global().read(|s| s.summaries().to_vec());
            panic!(
                "fresh execution thread {new_id} never surfaced in ThreadStore summaries \
                 (sidebar list) — deferred session file missed by every refresh\n\
                 sessions dir: {dir}\nfiles: {files:?}\nsummaries: {listed:?}",
                dir = dir.display(),
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    // The thread must be listed under the SAME id the facade thread carries
    // (the session file header id), so sidebar selection / running / unread
    // marks keyed by the facade id actually reach the row.
    let listed_id = manox_agent::thread_store_global().read(|s| {
        s.summaries()
            .iter()
            .find(|s| s.id == new_id)
            .map(|s| s.id.clone())
    });
    assert_eq!(
        listed_id.as_deref(),
        Some(new_id.as_str()),
        "the store summary id must equal the facade ThreadId — otherwise the \
         sidebar row is keyed by a different id and selection/running/unread \
         marks never reach it"
    );
    let summaries = manox_agent::thread_store_global().read(|s| s.summaries().to_vec());
    let summary = summaries
        .iter()
        .find(|s| s.id == new_id)
        .expect("thread present per the wait loop");
    assert!(
        summary.project == project.to_string_lossy(),
        "summary project should carry the bound project dir"
    );
    // Drop the process-global store entity so the gpui test app can tear
    // down without the leak detector tripping on it.
    manox_agent::thread_store::drop_global_for_test();
    eprintln!("PASS: fresh execution thread surfaced in store summaries: {summary:?}");
}
