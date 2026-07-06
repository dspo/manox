//! MCP (Model Context Protocol) client + tool bridge.
//!
//! A plugin may declare MCP servers via a root-level `.mcp.json`:
//! `{ "mcpServers": { "name": { "command": "...", "args": [...], "env": {...} } } }`.
//! At startup each declared server is spawned as a child process and spoken to
//! over newline-delimited JSON-RPC 2.0 on its stdio — the MCP stdio transport.
//! The server's exposed tools are bridged into manox's `AgentTool` trait so the
//! model can call them like any built-in tool.
//!
//! No `rmcp` dependency: MCP's wire is a thin JSON-RPC layer, and the project
//! prefers the standard library + `tokio` over a heavy SDK. The client
//! implements `initialize` → `tools/list` → `tools/call` directly. Failures
//! (server won't start, crashes mid-call, malformed response) surface as tool
//! errors fed back to the model — never as panics.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use anyhow::{Context as _, Result};
use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::Mutex;

use crate::plugin::PluginManager;
use crate::tool::{AgentTool, AnyAgentTool, ToolOutputSink};
use crate::tools::bridge_tokio;

/// One MCP server declared in a plugin's `.mcp.json`.
#[derive(Debug, Clone, Deserialize)]
pub struct McpServerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

/// Parsed `.mcp.json`.
#[derive(Debug, Clone, Deserialize, Default)]
struct McpFile {
    #[serde(default, rename = "mcpServers")]
    mcp_servers: HashMap<String, McpServerConfig>,
}

/// A tool exposed by an MCP server.
#[derive(Debug, Clone)]
pub struct McpToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// A live MCP server connection. stdin/stdout are mutex-guarded so concurrent
/// tool calls serialize on a single JSON-RPC channel (MCP stdio is
/// request/response — the server answers one `id` at a time).
pub struct McpClient {
    server_name: String,
    stdin: Arc<Mutex<ChildStdin>>,
    stdout: Arc<Mutex<BufReader<ChildStdout>>>,
    next_id: Mutex<i64>,
    /// Keeps the child alive for the connection's lifetime.
    _child: Child,
}

impl McpClient {
    /// Spawn the server process and perform the MCP handshake (`initialize` +
    /// `initialized` notification). The caller receives a ready client.
    pub async fn connect(server_name: String, cfg: &McpServerConfig) -> Result<Arc<Self>> {
        let mut cmd = tokio::process::Command::new(&cfg.command);
        cmd.args(&cfg.args);
        for (k, v) in &cfg.env {
            cmd.env(k, v);
        }
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawning MCP server `{server_name}` ({})", cfg.command))?;
        let stdin = child.stdin.take().context("MCP server stdin missing")?;
        let stdout = child.stdout.take().context("MCP server stdout missing")?;
        let client = Arc::new(Self {
            server_name: server_name.clone(),
            stdin: Arc::new(Mutex::new(stdin)),
            stdout: Arc::new(Mutex::new(BufReader::new(stdout))),
            next_id: Mutex::new(0),
            _child: child,
        });
        client.initialize().await?;
        Ok(client)
    }

    async fn initialize(&self) -> Result<()> {
        let _result = self
            .call(
                "initialize",
                serde_json::json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {"name": "manox", "version": env!("CARGO_PKG_VERSION")}
                }),
            )
            .await?;
        // The initialized notification has no id and no response; just send it.
        self.notify("notifications/initialized", serde_json::json!({}))
            .await?;
        Ok(())
    }

    pub async fn list_tools(&self) -> Result<Vec<McpToolDef>> {
        let result = self.call("tools/list", serde_json::json!({})).await?;
        let tools = result
            .get("tools")
            .and_then(|t| t.as_array())
            .context("tools/list response missing `tools` array")?;
        Ok(tools
            .iter()
            .filter_map(|t| {
                let name = t.get("name")?.as_str()?.to_string();
                let description = t
                    .get("description")
                    .and_then(|d| d.as_str())
                    .unwrap_or("")
                    .to_string();
                let input_schema = t
                    .get("inputSchema")
                    .cloned()
                    .unwrap_or(serde_json::json!({}));
                Some(McpToolDef {
                    name,
                    description,
                    input_schema,
                })
            })
            .collect())
    }

    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<String> {
        let result = self
            .call(
                "tools/call",
                serde_json::json!({"name": name, "arguments": arguments}),
            )
            .await?;
        // MCP returns content as a list of typed blocks; flatten text blocks.
        let content = result.get("content").and_then(|c| c.as_array());
        let mut out = String::new();
        if let Some(blocks) = content {
            for block in blocks {
                if block.get("type").and_then(|t| t.as_str()) == Some("text")
                    && let Some(text) = block.get("text").and_then(|t| t.as_str())
                {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(text);
                }
            }
        }
        if out.is_empty() {
            out = serde_json::to_string_pretty(&result).unwrap_or_default();
        }
        let is_error = result
            .get("isError")
            .and_then(|e| e.as_bool())
            .unwrap_or(false);
        if is_error {
            anyhow::bail!("{out}");
        }
        Ok(out)
    }

    /// Send a JSON-RPC request and await the matching response. Notifications
    /// emitted by the server in the meantime are logged and skipped — the loop
    /// reads until a response with our `id` arrives.
    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let id = {
            let mut g = self.next_id.lock().await;
            *g += 1;
            *g
        };
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let line = serde_json::to_string(&request)? + "\n";
        {
            let mut stdin = self.stdin.lock().await;
            stdin.write_all(line.as_bytes()).await?;
            stdin.flush().await?;
        }
        let mut stdout = self.stdout.lock().await;
        loop {
            let mut buf = String::new();
            let n = stdout.read_line(&mut buf).await?;
            if n == 0 {
                anyhow::bail!("MCP server `{}` closed stdout", self.server_name);
            }
            let trimmed = buf.trim();
            if trimmed.is_empty() {
                continue;
            }
            let msg: Value = match serde_json::from_str(trimmed) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(
                        server = self.server_name,
                        "ignoring non-JSON MCP line: {e}: {trimmed:?}"
                    );
                    continue;
                }
            };
            if msg.get("id").and_then(|i| i.as_i64()) == Some(id) {
                if let Some(err) = msg.get("error") {
                    anyhow::bail!("MCP error from `{}`: {}", self.server_name, err);
                }
                return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
            }
            // A notification or a response to a different (stale) id — drop it.
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        let line = serde_json::to_string(&msg)? + "\n";
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(line.as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }
}

/// One server's live client + the tools it exposed at handshake.
struct ReadyServer {
    client: Arc<McpClient>,
    tools: Vec<McpToolDef>,
}

/// Process-wide MCP registry: loads `.mcp.json` from installed plugins and
/// (asynchronously) connects each server. `ready_tools()` returns the
/// currently-connected servers' tools for registration into a `ToolRegistry`.
pub struct McpRegistry {
    ready: std::sync::Mutex<Vec<ReadyServer>>,
}

impl McpRegistry {
    pub fn load() -> Self {
        Self {
            ready: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Spawn a tokio task that connects every declared server and stores its
    /// tools. Failures are logged per-server (fail-open) — one unreachable MCP
    /// server never blocks the others or the app.
    pub fn start_all(&self) {
        let configs = Self::collect_configs();
        if configs.is_empty() {
            return;
        }
        let handle = crate::runtime::handle().clone();
        handle.spawn(async move {
            for (server_name, cfg, _plugin_root) in configs {
                let server_name_clone = server_name.clone();
                let client = match McpClient::connect(server_name.clone(), &cfg).await {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!("MCP server `{server_name}` connect failed: {e:#}");
                        continue;
                    }
                };
                let tools = match client.list_tools().await {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::warn!("MCP server `{server_name}` tools/list failed: {e:#}");
                        continue;
                    }
                };
                tracing::info!(
                    server = server_name_clone,
                    tools = tools.len(),
                    "MCP server ready"
                );
                if let Some(reg) = global_opt() {
                    reg.ready
                        .lock()
                        .expect("MCP registry poisoned")
                        .push(ReadyServer { client, tools });
                }
            }
        });
    }

    /// Collect `(server_name, config, plugin_root)` from every installed
    /// plugin's `.mcp.json`. Plugins without one contribute nothing.
    fn collect_configs() -> Vec<(String, McpServerConfig, PathBuf)> {
        let mut out = Vec::new();
        for plugin in PluginManager::installed() {
            let mcp_file = plugin.root.join(".mcp.json");
            let raw = match std::fs::read_to_string(&mcp_file) {
                Ok(r) => r,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    tracing::warn!("reading {}: {e}", mcp_file.display());
                    continue;
                }
            };
            let parsed: McpFile = match serde_json::from_str(&raw) {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!("parsing {}: {e}", mcp_file.display());
                    continue;
                }
            };
            for (name, cfg) in parsed.mcp_servers {
                out.push((name, cfg, plugin.root.clone()));
            }
        }
        out
    }

    /// Snapshot of currently-ready MCP tools as `AnyAgentTool` entries, ready to
    /// drop into a `ToolRegistry`. Servers that have not finished connecting
    /// contribute nothing this turn — they appear once their handshake completes
    /// and the next `Thread` is constructed.
    pub fn ready_tools(&self) -> Vec<AnyAgentTool> {
        let ready = self.ready.lock().expect("MCP registry poisoned");
        let mut out = Vec::new();
        for server in ready.iter() {
            let server_name = server.client.server_name.clone();
            for tool in &server.tools {
                out.push(
                    Arc::new(McpTool::new(server.client.clone(), &server_name, tool))
                        as AnyAgentTool,
                );
            }
        }
        out
    }
}

static REGISTRY: OnceLock<McpRegistry> = OnceLock::new();

pub fn init() {
    let registry = McpRegistry::load();
    if REGISTRY.set(registry).is_err() {
        tracing::warn!("MCP registry already initialized");
    }
    if let Some(reg) = global_opt() {
        reg.start_all();
    }
}

fn global_opt() -> Option<&'static McpRegistry> {
    REGISTRY.get()
}

/// MCP tools ready for registration into a fresh `ToolRegistry`.
pub fn ready_tools() -> Vec<AnyAgentTool> {
    global_opt().map(|r| r.ready_tools()).unwrap_or_default()
}

/// An `AgentTool` backed by an MCP server tool. The model sees a namespaced
/// name (`mcp__<server>__<tool>`) so MCP tools never collide with built-ins or
/// other servers; the original tool name is forwarded to the server on call.
pub struct McpTool {
    client: Arc<McpClient>,
    /// Original MCP tool name — what the server expects in `tools/call`.
    tool_name: String,
    /// Namespaced name exposed to the model and `ToolRegistry`.
    name: Arc<str>,
    description: String,
    input_schema: Value,
}

impl McpTool {
    pub fn new(client: Arc<McpClient>, server_name: &str, def: &McpToolDef) -> Self {
        let name: Arc<str> = format!("mcp__{server_name}__{}", def.name).into();
        Self {
            client,
            tool_name: def.name.clone(),
            name,
            description: def.description.clone(),
            input_schema: def.input_schema.clone(),
        }
    }
}

impl AgentTool for McpTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn input_schema(&self) -> serde_json::Value {
        self.input_schema.clone()
    }
    fn run(
        &self,
        input: serde_json::Value,
        cancel: tokio_util::sync::CancellationToken,
        cx: &mut gpui::App,
    ) -> gpui::Task<Result<String, String>> {
        let client = self.client.clone();
        let tool_name = self.tool_name.clone();
        bridge_tokio(cx, async move {
            if cancel.is_cancelled() {
                return Err(anyhow::anyhow!("MCP tool cancelled"));
            }
            client.call_tool(&tool_name, input).await
        })
    }
    fn run_streaming(
        &self,
        input: serde_json::Value,
        cancel: tokio_util::sync::CancellationToken,
        _sink: ToolOutputSink,
        cx: &mut gpui::App,
    ) -> gpui::Task<Result<String, String>> {
        self.run(input, cancel, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mcp_json() {
        let raw = r#"{
            "mcpServers": {
                "fs": {"command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem"]}
            }
        }"#;
        let f: McpFile = serde_json::from_str(raw).unwrap();
        assert_eq!(f.mcp_servers.len(), 1);
        assert_eq!(f.mcp_servers["fs"].command, "npx");
        assert_eq!(
            f.mcp_servers["fs"].args,
            vec!["-y", "@modelcontextprotocol/server-filesystem"]
        );
    }

    #[test]
    fn empty_mcp_file_is_default() {
        let f: McpFile = serde_json::from_str("{}").unwrap();
        assert!(f.mcp_servers.is_empty());
    }
}
