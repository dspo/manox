//! `LspClient`: one language server child process owned by the `supervisor`
//! bus, driven over hand-rolled JSON-RPC.
//!
//! The client owns no `Child` — `supervisor::spawn` keeps the `Child` in a
//! detached reaper task and hands back the piped stdin/stdout. The client
//! drives the writer half directly, and runs a reader task that demuxes
//! responses (by id, via `oneshot`) and caches notifications
//! (`publishDiagnostics`). Graceful shutdown is wired into the
//! `ManagedProcess` so a `shutdown`+`exit` pair runs before the bus's
//! `SIGTERM`→`SIGKILL` escalation.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, anyhow};
use lsp_types::Range;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::BufReader;
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::{Notify, oneshot};
use tracing::{debug, warn};

use crate::proto::{read_message, write_message};
use crate::spec::LspServerSpec;

/// Per-request deadline. LSP servers index large workspaces; a definition
/// call during indexing can take seconds, but hanging forever helps nobody.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Server lifecycle state. `Ready` is set once the `initialize` result lands —
/// the server is accepting requests. Indexing may still be in flight; a
/// code-intel call can return best-effort (possibly empty) results.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerStatus {
    NotStarted,
    Starting,
    Ready,
    Failed,
}

struct Inner {
    proc: Arc<supervisor::ManagedProcess>,
    writer: tokio::sync::Mutex<ChildStdin>,
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, Value>>>>,
    ready_notify: Arc<Notify>,
    status: Mutex<ServerStatus>,
    diagnostics: Mutex<HashMap<PathBuf, Vec<lsp_types::Diagnostic>>>,
    spec_id: &'static str,
    root: PathBuf,
}

/// A connected language server. Clone freely — the underlying state is shared
/// behind an `Arc`.
#[derive(Clone)]
pub struct LspClient {
    inner: Arc<Inner>,
}

/// Removes a pending-request entry on drop so a cancelled `request` future
/// (dropped before its timeout fires or the reader drains the map) doesn't
/// leave a dangling `oneshot::Sender` keyed by its id forever. On the paths
/// where the entry is already gone (response arrived → `dispatch` removed it;
/// reader died → the read loop's `std::mem::take` drained the map), the
/// guard's `remove` is a harmless no-op.
struct PendingGuard {
    id: u64,
    inner: Arc<Inner>,
}

impl PendingGuard {
    fn new(inner: Arc<Inner>, id: u64) -> Self {
        Self { inner, id }
    }
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.inner
            .pending
            .lock()
            .expect("pending mutex poisoned")
            .remove(&self.id);
    }
}

impl LspClient {
    /// Spawn the server process and start the reader task, returning a client in
    /// the `Starting` state. Does NOT run the `initialize` handshake — that is
    /// a separate [`initialize`] call so the registry can kick off spawning
    /// without blocking on the handshake (see `LspRegistry::ensure`).
    pub async fn start(spec: &'static LspServerSpec, root: PathBuf) -> anyhow::Result<Arc<Self>> {
        let name = format!("lsp-{}", spec.id);
        let mut cmd = tokio::process::Command::new(spec.spawn[0]);
        cmd.args(&spec.spawn[1..]);
        // rust-analyzer / gopls / pyright / ts-language-server all key workspace
        // state off `rootUri`, not the process cwd, but setting cwd matches what
        // a user running the server from the project root would get.
        cmd.current_dir(&root);
        let spawned = supervisor::global()
            .spawn(&name, cmd, supervisor::ProcessKind::Lsp)
            .await
            .map_err(|e| anyhow!("spawning LSP server `{}`: {e}", spec.id))?;

        let inner = Arc::new(Inner {
            proc: spawned.proc.clone(),
            writer: tokio::sync::Mutex::new(spawned.stdin),
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            ready_notify: Arc::new(Notify::new()),
            status: Mutex::new(ServerStatus::Starting),
            diagnostics: Mutex::new(HashMap::new()),
            spec_id: spec.id,
            root,
        });

        // Reader task: owns the stdout stream, demuxes responses and caches
        // notifications for the lifetime of the client.
        let reader_inner = inner.clone();
        let stdout: ChildStdout = spawned.stdout;
        tokio::spawn(async move {
            read_loop(reader_inner, stdout).await;
        });

        let client = Arc::new(Self { inner });
        client.set_graceful();
        Ok(client)
    }

    pub fn spec_id(&self) -> &'static str {
        self.inner.spec_id
    }

    pub fn status(&self) -> ServerStatus {
        *self.inner.status.lock().expect("status mutex poisoned")
    }

    pub fn is_ready(&self) -> bool {
        self.status() == ServerStatus::Ready
    }

    /// Wait up to `timeout` for the server to reach `Ready`. Returns the
    /// current status. Idempotent — returns immediately if already ready.
    pub async fn wait_ready(&self, timeout: Duration) -> ServerStatus {
        if self.is_ready() {
            return ServerStatus::Ready;
        }
        let _ = tokio::time::timeout(timeout, self.inner.ready_notify.notified()).await;
        self.status()
    }

    /// Send a request and await its `result` (or `error`). The reader task
    /// fulfils the `oneshot` when the matching id's response arrives.
    async fn request(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        // Guard removes the pending entry on every exit path — including a
        // dropped future (cancellation), where neither timeout nor the reader's
        // drain runs. On the response/reader-died paths the entry is already
        // gone, so the guard's `remove` is a no-op.
        let _guard = PendingGuard::new(self.inner.clone(), id);
        self.inner
            .pending
            .lock()
            .expect("pending mutex poisoned")
            .insert(id, tx);
        let msg = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let res = {
            let mut w = self.inner.writer.lock().await;
            write_message(&mut *w, &msg).await
        };
        if let Err(e) = res {
            return Err(anyhow!("writing `{method}` request: {e}"));
        }
        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(err))) => Err(anyhow!("`{method}` server error: {err}")),
            Ok(Err(_)) => {
                // Sender dropped — the reader task died. Surface as a hard
                // failure so the caller doesn't retry forever.
                self.mark_failed();
                Err(anyhow!(
                    "`{method}` response channel closed (server likely exited)"
                ))
            }
            Err(_) => Err(anyhow!(
                "`{method}` request timed out after {REQUEST_TIMEOUT:?}"
            )),
        }
    }

    async fn notify(&self, method: &str, params: Value) {
        let msg = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        let mut w = self.inner.writer.lock().await;
        if let Err(e) = write_message(&mut *w, &msg).await {
            warn!("LSP `{}` notify write failed: {e}", self.inner.spec_id);
        }
    }

    pub(crate) fn mark_failed(&self) {
        *self.inner.status.lock().expect("status mutex poisoned") = ServerStatus::Failed;
    }

    /// The `initialize` / `initialized` handshake. Declares client capabilities
    /// for the methods we use and lets the server watch files itself
    /// (`didChangeWatchedFiles`) — manox never sends did_open/did_change.
    ///
    /// Separate from [`start`](Self::start) so the registry can spawn the
    /// process without blocking on the handshake.
    pub async fn initialize(&self) -> anyhow::Result<()> {
        let root_uri = file_uri(&self.inner.root);
        let params = json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "rootPath": self.inner.root,
            "capabilities": {
                "workspace": {
                    "didChangeConfiguration": { "dynamicRegistration": false },
                    "didChangeWatchedFiles": { "dynamicRegistration": false },
                    "symbol": { "dynamicRegistration": false },
                },
                "textDocument": {
                    "synchronization": { "dynamicRegistration": false },
                    "definition": { "linkSupport": false },
                    "references": {},
                    "hover": { "contentFormat": ["markdown", "plaintext"] },
                    "documentSymbol": { "hierarchicalDocumentSymbolSupport": true },
                    "publishDiagnostics": { "relatedInformation": true }
                }
            },
            "workspaceFolders": [{ "uri": root_uri, "name": self.inner.root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default() }],
        });
        let result = self
            .request("initialize", params)
            .await
            .context("LSP initialize")?;
        debug!(
            spec = self.inner.spec_id,
            "initialize result: {}",
            truncate_debug(&result)
        );
        *self.inner.status.lock().expect("status mutex poisoned") = ServerStatus::Ready;
        self.inner.ready_notify.notify_waiters();
        self.notify("initialized", json!({})).await;
        Ok(())
    }

    /// Wire `shutdown`+`exit` into the process bus so the server closes its
    /// streams cleanly before the bus escalates to `SIGTERM`.
    fn set_graceful(&self) {
        let me = self.clone();
        self.inner.proc.set_graceful(Arc::new(move || {
            let me = me.clone();
            Box::pin(async move {
                // `shutdown` is a request (expect a result); `exit` is a
                // notification. Ignore errors — the bus will SIGTERM next.
                let _ = me.request("shutdown", Value::Null).await;
                me.notify("exit", Value::Null).await;
            })
        }));
    }

    // ---- code-intel requests ------------------------------------------------
    //
    // Position resolution: the caller gives a 1-indexed line and the symbol text.
    // The client reads just that line from disk and locates the symbol to compute
    // the 0-indexed LSP position. LSP `character` is a UTF-16 code-unit offset;
    // we approximate with a Unicode-scalar count (identical for ASCII code,
    // the only case where the difference matters in practice).

    fn resolve_position(
        path: &Path,
        line_1: u32,
        symbol: &str,
    ) -> anyhow::Result<lsp_types::Position> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("reading {} for position resolution", path.display()))?;
        let line = content
            .lines()
            .nth(line_1 as usize - 1)
            .ok_or_else(|| anyhow!("line {line_1} not in {}", path.display()))?;
        let col = line
            .find(symbol)
            .ok_or_else(|| anyhow!("symbol `{symbol}` not found on line {line_1}"))?;
        // char count up to the column ≈ LSP character offset for code.
        let character = line[..col].chars().count() as u32;
        Ok(lsp_types::Position {
            line: line_1 - 1,
            character,
        })
    }

    fn text_document_position(
        &self,
        path: &Path,
        line_1: u32,
        symbol: &str,
    ) -> anyhow::Result<Value> {
        let pos = Self::resolve_position(path, line_1, symbol)?;
        Ok(json!({
            "textDocument": { "uri": file_uri(path) },
            "position": pos,
        }))
    }

    pub async fn go_to_definition(
        &self,
        path: &Path,
        line_1: u32,
        symbol: &str,
    ) -> anyhow::Result<Vec<(PathBuf, u32, u32)>> {
        let params = self.text_document_position(path, line_1, symbol)?;
        let result = self.request("textDocument/definition", params).await?;
        Ok(parse_locations(result))
    }

    pub async fn find_references(
        &self,
        path: &Path,
        line_1: u32,
        symbol: &str,
        include_declaration: bool,
    ) -> anyhow::Result<Vec<(PathBuf, u32, u32)>> {
        let mut params = self.text_document_position(path, line_1, symbol)?;
        params["context"] = json!({ "includeDeclaration": include_declaration });
        let result = self.request("textDocument/references", params).await?;
        Ok(parse_locations(result))
    }

    pub async fn hover(
        &self,
        path: &Path,
        line_1: u32,
        symbol: &str,
    ) -> anyhow::Result<Option<String>> {
        let params = self.text_document_position(path, line_1, symbol)?;
        let result = self.request("textDocument/hover", params).await?;
        Ok(parse_hover(result))
    }

    pub async fn document_symbols(
        &self,
        path: &Path,
    ) -> anyhow::Result<Vec<(String, String, u32)>> {
        let params = json!({ "textDocument": { "uri": file_uri(path) } });
        let result = self.request("textDocument/documentSymbol", params).await?;
        Ok(parse_document_symbols(result))
    }

    pub async fn workspace_symbols(
        &self,
        query: &str,
    ) -> anyhow::Result<Vec<(PathBuf, String, u32, u32)>> {
        let params = json!({ "query": query });
        let result = self.request("workspace/symbol", params).await?;
        Ok(parse_workspace_symbols(result))
    }

    /// Cached `publishDiagnostics` for a file, if any have been pushed. The
    /// server pushes these asynchronously as it indexes; an empty vec means
    /// either no diagnostics or none pushed yet (the distinction is surfaced
    /// to the caller as a staleness note, not here).
    pub fn cached_diagnostics(&self, path: &Path) -> Vec<lsp_types::Diagnostic> {
        self.inner
            .diagnostics
            .lock()
            .expect("diagnostics mutex poisoned")
            .get(path)
            .cloned()
            .unwrap_or_default()
    }

    /// Every file path with cached `publishDiagnostics`, for the
    /// whole-project `Diagnostics` tool (no `path` argument).
    pub fn cached_diagnostic_files(&self) -> Vec<PathBuf> {
        self.inner
            .diagnostics
            .lock()
            .expect("diagnostics mutex poisoned")
            .keys()
            .cloned()
            .collect()
    }
}

/// Reader loop: frame messages off the server stdout and demux.
async fn read_loop(inner: Arc<Inner>, stdout: ChildStdout) {
    let mut reader = BufReader::new(stdout);
    loop {
        match read_message(&mut reader).await {
            Ok(Some(msg)) => dispatch(&inner, msg),
            Ok(None) => {
                debug!(spec = inner.spec_id, "LSP server stdout EOF");
                break;
            }
            Err(e) => {
                warn!(spec = inner.spec_id, "LSP read error: {e}");
                break;
            }
        }
    }
    // Server stream gone — fail any in-flight requests and mark not-ready.
    *inner.status.lock().expect("status mutex poisoned") = ServerStatus::Failed;
    let pending = std::mem::take(&mut *inner.pending.lock().expect("pending mutex poisoned"));
    for (_id, tx) in pending {
        let _ = tx.send(Err(
            json!({ "code": -1, "message": "server stream closed" }),
        ));
    }
}

/// Route one inbound message: a response fulfils a pending request; a
/// notification updates cache/state.
fn dispatch(inner: &Arc<Inner>, msg: Value) {
    if let Some(id) = msg.get("id").cloned() {
        // Response (request or error) — match by id.
        let id = match id {
            Value::Number(n) => n.as_u64(),
            Value::String(s) => s.parse().ok(),
            _ => None,
        };
        if let Some(id) = id {
            let tx = inner
                .pending
                .lock()
                .expect("pending mutex poisoned")
                .remove(&id);
            if let Some(tx) = tx {
                if let Some(err) = msg.get("error") {
                    let _ = tx.send(Err(err.clone()));
                } else {
                    let _ = tx.send(Ok(msg.get("result").cloned().unwrap_or(Value::Null)));
                }
            } else {
                debug!(spec = inner.spec_id, "response for unknown id {id}");
            }
        }
        return;
    }
    // Notification.
    let method = match msg.get("method").and_then(|m| m.as_str()) {
        Some(m) => m,
        None => return,
    };
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    match method {
        "textDocument/publishDiagnostics" => {
            let parsed: PublishDiagnosticsParams = match serde_json::from_value(params) {
                Ok(p) => p,
                Err(e) => {
                    warn!(spec = inner.spec_id, "malformed publishDiagnostics: {e}");
                    return;
                }
            };
            if let Some(path) = uri_to_path(&parsed.uri) {
                inner
                    .diagnostics
                    .lock()
                    .expect("diagnostics mutex poisoned")
                    .insert(path, parsed.diagnostics);
            }
        }
        "window/logMessage" => {
            debug!(
                spec = inner.spec_id,
                "server logMessage: {}",
                truncate_debug(&params)
            );
        }
        "$/progress" => {
            debug!(
                spec = inner.spec_id,
                "$/progress: {}",
                truncate_debug(&params)
            );
        }
        _ => {
            debug!(spec = inner.spec_id, "unhandled notification {method}");
        }
    }
}

#[derive(Deserialize)]
struct PublishDiagnosticsParams {
    uri: String,
    diagnostics: Vec<lsp_types::Diagnostic>,
}

// ---- output parsing helpers ------------------------------------------------

/// `textDocument/definition` / `references` results: a single `Location`, an
/// array of `Location`s, or `null`. Flatten to `(path, line, col)` triples
/// (1-indexed line/col for direct feeding to `read_file`).
fn parse_locations(result: Value) -> Vec<(PathBuf, u32, u32)> {
    let locs: Vec<RawLocation> = match result {
        Value::Null => return Vec::new(),
        Value::Array(_) => serde_json::from_value(result).unwrap_or_default(),
        single => serde_json::from_value(single).into_iter().collect(),
    };
    locs.into_iter()
        .filter_map(|l| {
            let path = uri_to_path(&l.uri)?;
            Some((path, l.range.start.line + 1, l.range.start.character + 1))
        })
        .collect()
}

#[derive(Deserialize)]
struct RawLocation {
    uri: String,
    range: Range,
}

/// `textDocument/hover` result: `{ contents, range? }` or `null`. `contents`
/// is a `MarkupContent` `{ kind, value }`, a `MarkedString`, or an array of
/// either. Extract whatever markdown/plaintext strings are present.
fn parse_hover(result: Value) -> Option<String> {
    if result.is_null() {
        return None;
    }
    let contents = result.get("contents")?;
    let mut out = Vec::new();
    extract_hover_text(contents, &mut out);
    if out.is_empty() {
        None
    } else {
        Some(out.join("\n\n"))
    }
}

fn extract_hover_text(contents: &Value, out: &mut Vec<String>) {
    match contents {
        Value::String(s) => out.push(s.clone()),
        Value::Object(map) => {
            if let Some(Value::String(s)) = map.get("value") {
                out.push(s.clone());
            }
        }
        Value::Array(arr) => {
            for item in arr {
                extract_hover_text(item, out);
            }
        }
        _ => {}
    }
}

/// `textDocument/documentSymbol` result: an array of `DocumentSymbol` (hierarchical)
/// or `SymbolInformation`. Flatten the tree to `(name, kind_name, line)`.
fn parse_document_symbols(result: Value) -> Vec<(String, String, u32)> {
    let mut out = Vec::new();
    if let Value::Array(arr) = result {
        for item in arr {
            flatten_symbols(&item, &mut out);
        }
    }
    out
}

fn flatten_symbols(item: &Value, out: &mut Vec<(String, String, u32)>) {
    let name = item
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let kind = symbol_kind_name(item.get("kind").and_then(|v| v.as_u64()).unwrap_or(0));
    // `selectionRange` (DocumentSymbol — a Range directly) or `location.range`
    // (SymbolInformation — a `{ uri, range }` object). Both shapes carry the
    // symbol's start position under `start`.
    let line = item
        .get("selectionRange")
        .and_then(|r| r.get("start"))
        .or_else(|| {
            item.get("location")
                .and_then(|l| l.get("range"))
                .and_then(|r| r.get("start"))
        })
        .and_then(|s| s.get("line"))
        .and_then(|l| l.as_u64())
        .unwrap_or(0) as u32;
    if !name.is_empty() {
        out.push((name, kind, line + 1));
    }
    if let Some(children) = item.get("children").and_then(|c| c.as_array()) {
        for child in children {
            flatten_symbols(child, out);
        }
    }
}

fn symbol_kind_name(kind: u64) -> String {
    match kind {
        1 => "file".into(),
        2 => "module".into(),
        3 => "namespace".into(),
        4 => "package".into(),
        5 => "class".into(),
        6 => "method".into(),
        7 => "property".into(),
        8 => "field".into(),
        9 => "constructor".into(),
        12 => "function".into(),
        13 => "variable".into(),
        14 => "constant".into(),
        23 => "struct".into(),
        24 => "event".into(),
        25 => "operator".into(),
        26 => "interface".into(),
        _ => format!("kind{kind}"),
    }
}

/// `workspace/symbol` result: an array of `SymbolInformation`
/// `{ name, kind, location: { uri, range } }`.
fn parse_workspace_symbols(result: Value) -> Vec<(PathBuf, String, u32, u32)> {
    let arr = match result {
        Value::Array(arr) => arr,
        _ => return Vec::new(),
    };
    arr.into_iter()
        .filter_map(|s| {
            let name = s.get("name")?.as_str()?.to_string();
            let location = s.get("location")?;
            let uri = location.get("uri")?.as_str()?;
            let path = uri_to_path(uri)?;
            let line = location.get("range")?.get("start")?.get("line")?.as_u64()? as u32;
            let col = location
                .get("range")?
                .get("start")?
                .get("character")?
                .as_u64()? as u32;
            Some((path, name, line + 1, col + 1))
        })
        .collect()
}

// ---- URI helpers -----------------------------------------------------------
//
// LSP `file:` URIs use percent-encoding for non-ASCII path bytes. We hand-roll
// the decode to avoid pulling the `url` crate; the common case (ASCII paths
// on macOS) round-trips without any encoded bytes.

fn file_uri(path: &Path) -> String {
    let abs = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let s = abs.to_string_lossy();
    if cfg!(target_os = "windows") {
        format!("file:///{}", s.trim_start_matches('/'))
    } else {
        format!("file://{s}")
    }
}

fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let decoded = percent_decode(rest);
    Some(PathBuf::from(decoded))
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push(h << 4 | l);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| input.to_string())
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn truncate_debug(v: &Value) -> String {
    let s = v.to_string();
    if s.len() > 200 {
        format!("{}…", &s[..200])
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Throwaway live handshake check: spawn → initialize → Ready → graceful
    /// close. Uses gopls (a real binary, not a rustup proxy). Gated off by
    /// default. Run with `MANOX_RUN_LIVE=1 cargo test -p lsp live_handshake
    /// -- --nocapture`.
    #[cfg(unix)]
    #[tokio::test]
    async fn live_handshake_gopls() {
        if std::env::var("MANOX_RUN_LIVE").is_err() {
            return;
        }
        let spec = crate::spec::spec_for_id("gopls").expect("spec");
        let root = std::env::current_dir().unwrap();
        let client = LspClient::start(spec, root.clone()).await.expect("start");
        client.initialize().await.expect("initialize");
        assert!(client.is_ready(), "client should be Ready after initialize");
        // Graceful close runs the LSP shutdown/exit hook; the supervisor reaps.
        client.inner.proc.close().await;
        assert!(
            client.inner.proc.is_exited(),
            "gopls should exit after graceful close"
        );
    }

    /// Throwaway live code-intel round-trip: stand up a temp Go module, let
    /// gopls index it, run `documentSymbol`, and confirm a known symbol is
    /// returned. Validates the full request/parse path against a real server.
    /// Gated off by default.
    #[cfg(unix)]
    #[tokio::test]
    async fn live_document_symbols_gopls() {
        if std::env::var("MANOX_RUN_LIVE").is_err() {
            return;
        }
        let tmp = std::env::temp_dir().join(format!("manox-lsp-live-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("go.mod"), "module manoxlive\ngo 1.21\n").unwrap();
        std::fs::write(
            tmp.join("main.go"),
            "package main\n\nfunc Greet() string { return \"hi\" }\nfunc main() { _ = Greet() }\n",
        )
        .unwrap();
        let spec = crate::spec::spec_for_id("gopls").expect("spec");
        let client = LspClient::start(spec, tmp.clone()).await.expect("start");
        client.initialize().await.expect("initialize");
        // gopls indexes asynchronously after initialize; poll documentSymbol
        // until it returns the `Greet` symbol (or time out).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        let mut got_greet = false;
        loop {
            let syms = client
                .document_symbols(&tmp.join("main.go"))
                .await
                .unwrap_or_default();
            if syms.iter().any(|(name, _, _)| name == "Greet") {
                got_greet = true;
                break;
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
        client.inner.proc.close().await;
        let _ = std::fs::remove_dir_all(&tmp);
        assert!(got_greet, "gopls should report the `Greet` symbol");
    }

    #[test]
    fn file_uri_round_trip() {
        let path = PathBuf::from("/tmp/manox-test.rs");
        let uri = file_uri(&path);
        assert!(uri.starts_with("file://"));
        let back = uri_to_path(&uri).unwrap();
        // canonicalize may resolve /tmp, so compare loosely by suffix.
        assert!(back.to_string_lossy().ends_with("manox-test.rs"));
    }

    #[test]
    fn uri_to_path_decodes_percent() {
        let p = uri_to_path("file:///Users/x/hello%20world.rs").unwrap();
        assert_eq!(p.to_string_lossy(), "/Users/x/hello world.rs");
    }

    #[test]
    fn parse_locations_handles_null() {
        assert!(parse_locations(Value::Null).is_empty());
    }

    #[test]
    fn parse_locations_single_and_array() {
        let single = json!({
            "uri": "file:///x/a.rs",
            "range": { "start": { "line": 3, "character": 5 }, "end": { "line": 3, "character": 8 } }
        });
        let v = parse_locations(single.clone());
        assert_eq!(v, vec![(PathBuf::from("/x/a.rs"), 4, 6)]);

        let arr = json!([single, {
            "uri": "file:///x/b.rs",
            "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } }
        }]);
        let v = parse_locations(arr);
        assert_eq!(v.len(), 2);
    }

    #[test]
    fn parse_hover_markup_content() {
        let result = json!({ "contents": { "kind": "markdown", "value": "fn foo()" } });
        assert_eq!(parse_hover(result).unwrap(), "fn foo()");
    }

    #[test]
    fn parse_hover_marked_string_array() {
        let result = json!({ "contents": ["a", { "language": "rust", "value": "b" }] });
        assert_eq!(parse_hover(result).unwrap(), "a\n\nb");
    }

    #[test]
    fn parse_hover_null() {
        assert!(parse_hover(Value::Null).is_none());
    }

    #[test]
    fn document_symbols_flatten_children() {
        let result = json!([{
            "name": "root",
            "kind": 23,
            "selectionRange": { "start": { "line": 1, "character": 0 }, "end": { "line": 1, "character": 4 } },
            "children": [{
                "name": "child",
                "kind": 12,
                "selectionRange": { "start": { "line": 2, "character": 0 }, "end": { "line": 2, "character": 5 } },
                "children": []
            }]
        }]);
        let v = parse_document_symbols(result);
        assert_eq!(v[0], ("root".into(), "struct".into(), 2));
        assert_eq!(v[1], ("child".into(), "function".into(), 3));
    }

    #[test]
    fn workspace_symbols_parse() {
        let result = json!([{
            "name": "foo",
            "kind": 6,
            "location": {
                "uri": "file:///x/a.rs",
                "range": { "start": { "line": 10, "character": 4 }, "end": { "line": 10, "character": 7 } }
            }
        }]);
        let v = parse_workspace_symbols(result);
        assert_eq!(v, vec![(PathBuf::from("/x/a.rs"), "foo".into(), 11, 5)]);
    }
}
