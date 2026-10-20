//! A minimal scripted HTTP/1.1 server for `web_fetch` transport tests:
//! per-path scripted responses plus per-path request counters and heads, so
//! tests can assert not only outcomes but also that a forbidden destination was
//! never contacted and what the request actually carried.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// The hostname the scripted server answers for. Production refuses loopback
/// *literals*, so a transport test addresses the server by name and lets its
/// resolver point that name at the loopback socket.
pub(super) const SERVER_HOST: &str = "loopback.test";

#[derive(Clone)]
pub(super) struct ScriptedResponse {
    pub(super) status: u16,
    pub(super) headers: Vec<(String, String)>,
    pub(super) body: Vec<u8>,
    /// Accept and read the request, then never answer (timeout tests).
    pub(super) hang: bool,
    /// Answer without a `Content-Length`, so the body is delimited by the
    /// closed connection — the shape a server uses to under-report its length.
    pub(super) omit_content_length: bool,
}

pub(super) fn ok(content_type: &str, body: impl Into<Vec<u8>>) -> ScriptedResponse {
    ScriptedResponse {
        status: 200,
        headers: vec![("Content-Type".into(), content_type.into())],
        body: body.into(),
        hang: false,
        omit_content_length: false,
    }
}

/// A `200` whose body length is not declared up front.
pub(super) fn undeclared(content_type: &str, body: impl Into<Vec<u8>>) -> ScriptedResponse {
    ScriptedResponse {
        omit_content_length: true,
        ..ok(content_type, body)
    }
}

pub(super) fn redirect(status: u16, location: &str) -> ScriptedResponse {
    ScriptedResponse {
        status,
        headers: vec![("Location".into(), location.into())],
        body: vec![],
        hang: false,
        omit_content_length: false,
    }
}

impl ScriptedResponse {
    pub(super) fn with_status(mut self, status: u16) -> Self {
        self.status = status;
        self
    }
}

type Script = Arc<Mutex<HashMap<String, ScriptedResponse>>>;
type Counts = Arc<Mutex<HashMap<String, usize>>>;
type Heads = Arc<Mutex<HashMap<String, String>>>;

pub(super) struct TestServer {
    pub(super) addr: SocketAddr,
    script: Script,
    counts: Counts,
    heads: Heads,
}

impl TestServer {
    pub(super) async fn start() -> TestServer {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral loopback port");
        let addr = listener.local_addr().expect("local addr");
        let script: Script = Arc::new(Mutex::new(HashMap::new()));
        let counts: Counts = Arc::new(Mutex::new(HashMap::new()));
        let heads: Heads = Arc::new(Mutex::new(HashMap::new()));
        let task_script = Arc::clone(&script);
        let task_counts = Arc::clone(&counts);
        let task_heads = Arc::clone(&heads);
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let script = Arc::clone(&task_script);
                let counts = Arc::clone(&task_counts);
                let heads = Arc::clone(&task_heads);
                tokio::spawn(handle(stream, script, counts, heads));
            }
        });
        TestServer {
            addr,
            script,
            counts,
            heads,
        }
    }

    pub(super) fn respond(&self, path: &str, response: ScriptedResponse) {
        self.script.lock().insert(path.to_string(), response);
    }

    /// Number of requests the server actually received for `path`.
    pub(super) fn count(&self, path: &str) -> usize {
        *self.counts.lock().get(&key(path)).unwrap_or(&0)
    }

    /// The request head the server received for `path`, when it received one.
    pub(super) fn last_request_head(&self, path: &str) -> Option<String> {
        self.heads.lock().get(&key(path)).cloned()
    }

    /// A resource on this server, addressed by name (see [`SERVER_HOST`]).
    pub(super) fn host_url(&self, path: &str) -> String {
        format!("http://{SERVER_HOST}:{}/{}", self.addr.port(), path)
    }
}

/// Requests are keyed by their target: an origin-form path, or the
/// absolute-form URL a client sends to a proxy.
fn key(target: &str) -> String {
    if target.starts_with('/') || target.contains("://") {
        target.to_string()
    } else {
        format!("/{target}")
    }
}

async fn handle(mut stream: TcpStream, script: Script, counts: Counts, heads: Heads) {
    // Request head only: GET carries no body.
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 16 * 1024 {
                    break;
                }
            }
        }
    }
    let head = String::from_utf8_lossy(&buf);
    let path = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/")
        .to_string();
    *counts.lock().entry(path.clone()).or_insert(0) += 1;
    heads.lock().insert(path.clone(), head.to_string());
    let Some(resp) = script.lock().get(&path).cloned() else {
        return;
    };
    if resp.hang {
        std::future::pending::<()>().await;
    }
    let mut out = format!(
        "HTTP/1.1 {} {}\r\n",
        resp.status,
        reason_phrase(resp.status)
    );
    for (k, v) in &resp.headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    if !resp.omit_content_length {
        out.push_str(&format!("Content-Length: {}\r\n", resp.body.len()));
    }
    out.push_str("Connection: close\r\n\r\n");
    let _ = stream.write_all(out.as_bytes()).await;
    let _ = stream.write_all(&resp.body).await;
    let _ = stream.flush().await;
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        301 => "Moved Permanently",
        302 => "Found",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "OK",
    }
}
