//! A minimal scripted HTTP/1.1 server for `web_fetch` transport tests:
//! per-path scripted responses plus per-path request counters, so tests can
//! assert not only outcomes but also that a forbidden destination was never
//! contacted.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[derive(Clone)]
pub(super) struct ScriptedResponse {
    pub(super) status: u16,
    pub(super) headers: Vec<(String, String)>,
    pub(super) body: Vec<u8>,
    /// Accept and read the request, then never answer (timeout tests).
    pub(super) hang: bool,
}

pub(super) fn ok(content_type: &str, body: impl Into<Vec<u8>>) -> ScriptedResponse {
    ScriptedResponse {
        status: 200,
        headers: vec![("Content-Type".into(), content_type.into())],
        body: body.into(),
        hang: false,
    }
}

pub(super) fn redirect(status: u16, location: &str) -> ScriptedResponse {
    ScriptedResponse {
        status,
        headers: vec![("Location".into(), location.into())],
        body: vec![],
        hang: false,
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

pub(super) struct TestServer {
    pub(super) addr: SocketAddr,
    script: Script,
    counts: Counts,
}

impl TestServer {
    pub(super) async fn start() -> TestServer {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral loopback port");
        let addr = listener.local_addr().expect("local addr");
        let script: Script = Arc::new(Mutex::new(HashMap::new()));
        let counts: Counts = Arc::new(Mutex::new(HashMap::new()));
        let task_script = Arc::clone(&script);
        let task_counts = Arc::clone(&counts);
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let script = Arc::clone(&task_script);
                let counts = Arc::clone(&task_counts);
                tokio::spawn(handle(stream, script, counts));
            }
        });
        TestServer {
            addr,
            script,
            counts,
        }
    }

    pub(super) fn respond(&self, path: &str, response: ScriptedResponse) {
        self.script.lock().insert(path.to_string(), response);
    }

    /// Number of requests the server actually received for `path`.
    pub(super) fn count(&self, path: &str) -> usize {
        let key = if path.starts_with('/') {
            path.to_string()
        } else {
            format!("/{path}")
        };
        *self.counts.lock().get(&key).unwrap_or(&0)
    }

    pub(super) fn url(&self, path: &str) -> String {
        format!("http://{}/{}", self.addr, path)
    }
}

async fn handle(mut stream: TcpStream, script: Script, counts: Counts) {
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
    out.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        resp.body.len()
    ));
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
