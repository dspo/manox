//! Headless AHP host launcher — local verification tooling.
//!
//! ```bash
//! cargo run -p manox-session-core --example ahp_serve -- --port 8765
//! ```
//!
//! It brings up the same loopback gateway `cx web` starts (`manox_agent::init`
//! plus `ws::start`: one listener, one per-boot token, one machine lock) and
//! prints the two endpoints that listener serves:
//!
//! - `/ahp` — the AHP (v3) face: JSON-RPC over one text frame per message.
//!   Point an AHP client at `<url>ahp?token=<token>` (VS Code's agent window,
//!   AHPX, or the companion smoke client in `manox-ahp`:
//!   `cargo run -p manox-ahp --example ahp_smoke`).
//! - `/ws` — the retiring v2 face, kept until the desktop app is switched over.
//!
//! The token is printed because an out-of-process client needs it; the same
//! value is written to `<config>/gateway-ws.json` (mode 0600) for tooling that
//! would rather read it from disk.

use std::time::Duration;

fn main() {
    let mut port: u16 = 0;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--port" => {
                port = args
                    .next()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or_else(|| {
                        eprintln!("--port needs a number (0 picks a free one)");
                        std::process::exit(2);
                    });
            }
            "--help" | "-h" => {
                println!("usage: ahp_serve [--port <u16>]  (0 = pick a free loopback port)");
                return;
            }
            other => {
                eprintln!("unknown argument {other:?} (try --help)");
                std::process::exit(2);
            }
        }
    }

    // A verification run must be able to see gateway-side failures; without a
    // subscriber the tracing events the router emits go nowhere.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "manox_ahp=debug,manox_session_core=info".into()),
        )
        .init();
    manox_agent::init();
    let cwd = manox_session_core::ws::default_cwd();
    manox_session_core::ws::start(cwd.clone(), port);

    // The bind is asynchronous; wait for the published endpoint rather than
    // printing an address no client could reach.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let endpoint = loop {
        if let Some(endpoint) = manox_session_core::ws::service_endpoint() {
            break endpoint;
        }
        if std::time::Instant::now() > deadline {
            eprintln!("gateway did not publish an endpoint within 10s (see the log above)");
            std::process::exit(1);
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    println!("cwd        {}", cwd.display());
    println!("clients    {}", endpoint.url);
    println!(
        "ahp        ws://127.0.0.1:{}/ahp?token={}",
        endpoint.port, endpoint.token
    );
    println!(
        "v2 (retiring) ws://127.0.0.1:{}/ws?token={}",
        endpoint.port, endpoint.token
    );
    println!();
    println!("smoke test: cargo run -p manox-ahp --example ahp_smoke -- \\");
    println!(
        "              --url ws://127.0.0.1:{}/ahp?token={}",
        endpoint.port, endpoint.token
    );
    println!("ctrl-c to stop the host.");

    // The listener runs on the global runtime; park this thread for the life of
    // the process.
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}
