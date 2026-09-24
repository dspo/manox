//! AHP smoke client — local verification tooling.
//!
//! ```bash
//! # against a host started with `--example ahp_serve`
//! cargo run -p manox-ahp --example ahp_smoke -- --url ws://127.0.0.1:8765/ahp?token=…
//!
//! # or read the token from the host's endpoint file and prompt a real turn
//! cargo run -p manox-ahp --example ahp_smoke -- --from-file --prompt "say hi"
//! ```
//!
//! What it does, in AHP terms: `initialize` (subscribing the root channel),
//! print the agent/model catalogue and the `x-manox` declaration, `listSessions`,
//! optionally `createSession` + subscribe that session and its default chat, then
//! optionally dispatch one `chat/turnStarted` and print every action envelope the
//! host publishes in reply — the client half of the protocol, end to end.

use std::time::Duration;

use ahp::{Client, ClientConfig, SubscriptionEvent};
use ahp_types::commands::{CreateSessionParams, ListSessionsParams};
use ahp_types::common::ROOT_RESOURCE_URI;
use ahp_types::version::PROTOCOL_VERSION;

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let mut url: Option<String> = None;
    let mut from_file = false;
    let mut prompt: Option<String> = None;
    let mut create = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--url" => url = args.next(),
            "--prompt" => prompt = args.next(),
            "--create" => create = true,
            "--from-file" => from_file = true,
            "--help" | "-h" => {
                println!(
                    "usage: ahp_smoke [--url <ws url>] [--from-file] [--create] [--prompt <text>]"
                );
                return;
            }
            other => {
                eprintln!("unknown argument {other:?} (try --help)");
                std::process::exit(2);
            }
        }
    }
    if from_file {
        url = Some(ahp_url_from_endpoint_file());
    }
    let Some(url) = url else {
        eprintln!("pass --url <ws url> (or --from-file to read the published endpoint)");
        std::process::exit(2);
    };

    let transport = match ahp_ws::WebSocketTransport::connect(&url).await {
        Ok(transport) => transport,
        Err(error) => {
            eprintln!("cannot reach {url}: {error}");
            std::process::exit(1);
        }
    };
    let client = Client::connect(transport, ClientConfig::default())
        .await
        .expect("transport handshake");
    let init = client
        .initialize(
            "ahp_smoke".to_string(),
            vec![PROTOCOL_VERSION.to_string()],
            vec![ROOT_RESOURCE_URI.to_string()],
        )
        .await
        .expect("initialize");
    println!(
        "connected: protocol {} serverSeq {}",
        init.protocol_version, init.server_seq
    );

    if let Some(state) = init.snapshots.first() {
        let value = serde_json::to_value(&state.state).expect("snapshot serializes");
        let agents = value["agents"].as_array().cloned().unwrap_or_default();
        println!("root: {} agent(s)", agents.len());
        for agent in &agents {
            let models = agent["models"].as_array().cloned().unwrap_or_default();
            println!(
                "  {} ({} model(s)): {}",
                agent["provider"].as_str().unwrap_or("?"),
                models.len(),
                models
                    .iter()
                    .filter_map(|model| model["id"].as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        if let Some(meta) = value.get("_meta") {
            println!(
                "  extension surface: {}",
                meta["x-manox"]["version"].as_u64().unwrap_or_default()
            );
        }
    }

    let sessions: ahp_types::commands::ListSessionsResult = client
        .request(
            "listSessions",
            ListSessionsParams {
                channel: ROOT_RESOURCE_URI.to_string(),
                meta: None,
                limit: Some(10),
                cursor: None,
            },
        )
        .await
        .expect("listSessions");
    println!("sessions: {}", sessions.items.len());
    for session in &sessions.items {
        println!("  {} — {}", session.resource, session.title);
    }

    let mut root_sub = client.attach_subscription(ROOT_RESOURCE_URI).await;
    let target = if create || sessions.items.is_empty() {
        let id = uuid::Uuid::new_v4().to_string();
        let session_uri = format!("ahp-session:/{id}");
        let _created: serde_json::Value = client
            .request(
                "createSession",
                CreateSessionParams {
                    channel: session_uri.clone(),
                    meta: None,
                    provider: None,
                    working_directories: None,
                    config: None,
                    active_client: None,
                    progress_token: None,
                },
            )
            .await
            .expect("createSession");
        println!("created {session_uri}");
        session_uri
    } else {
        sessions.items[0].resource.clone()
    };

    let (session_snapshot, mut session_sub) = client
        .subscribe(target.clone())
        .await
        .expect("subscribe session");
    let default_chat = session_snapshot
        .snapshot
        .as_ref()
        .map(|snapshot| serde_json::to_value(&snapshot.state).expect("snapshot serializes"))
        .and_then(|value| {
            value
                .get("defaultChat")
                .and_then(|chat| chat.as_str())
                .map(str::to_string)
        });
    println!("subscribed {target}, defaultChat {default_chat:?}");

    let mut chat_sub = match &default_chat {
        Some(chat) => {
            let (snapshot, sub) = client
                .subscribe(chat.clone())
                .await
                .expect("subscribe chat");
            let turns = snapshot
                .snapshot
                .as_ref()
                .map(|snapshot| serde_json::to_value(&snapshot.state).expect("snapshot serializes"))
                .and_then(|value| value["turns"].as_array().map(Vec::len))
                .unwrap_or_default();
            println!("chat snapshot: {turns} turn(s)");
            Some(sub)
        }
        None => None,
    };

    if let Some(prompt) = prompt {
        let Some(chat) = default_chat else {
            eprintln!("the session has no default chat to prompt");
            std::process::exit(1);
        };
        let action = serde_json::from_value(serde_json::json!({
            "type": "chat/turnStarted",
            "turnId": format!("t-{}", uuid::Uuid::new_v4()),
            "message": {"text": prompt, "origin": {"kind": "user"}},
        }))
        .expect("turnStarted shape");
        client.dispatch(chat, action).await.expect("dispatch");
        println!("prompt dispatched; watching the action stream for 120s (ctrl-c to stop)");

        if let Some(sub) = chat_sub.as_mut() {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
            loop {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    break;
                }
                match tokio::time::timeout(remaining, sub.recv()).await {
                    Ok(Some(SubscriptionEvent::Action(envelope))) => {
                        let tag = serde_json::to_value(&envelope.action)
                            .ok()
                            .and_then(|value| value["type"].as_str().map(str::to_string))
                            .unwrap_or_default();
                        let rejected = envelope
                            .rejection_reason
                            .as_deref()
                            .map(|reason| format!(" REJECTED: {reason}"))
                            .unwrap_or_default();
                        println!("  [{}] {tag}{rejected}", envelope.server_seq);
                    }
                    Ok(Some(other)) => println!("  {other:?}"),
                    Ok(None) | Err(_) => break,
                }
            }
        }
        return;
    }

    // No prompt: show that this connection holds its subscriptions, then stop.
    // Every wait is bounded — a smoke client that can hang would hide the very
    // failures it exists to surface.
    println!("holding subscriptions for 5s…");
    let _ = tokio::time::timeout(Duration::from_secs(5), root_sub.recv()).await;
    let _ = tokio::time::timeout(Duration::from_secs(1), session_sub.recv()).await;
    println!("done");
}

/// The AHP URL derived from the host's published endpoint file
/// (`<config>/gateway-ws.json`, mode 0600).
fn ahp_url_from_endpoint_file() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let path = std::path::PathBuf::from(home).join(".manox/gateway-ws.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        eprintln!("cannot read {} (is the host running?)", path.display());
        std::process::exit(2);
    };
    let value: serde_json::Value = serde_json::from_str(&text).expect("endpoint file is JSON");
    let port = value["port"].as_u64().unwrap_or_default();
    let token = value["token"].as_str().unwrap_or_default();
    format!("ws://127.0.0.1:{port}/ahp?token={token}")
}
