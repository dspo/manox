//! Process-global MCP registry.
//!
//! Mirrors `ProviderRegistry`: `init()` loads `~/.config/cx/manox/mcp.toml`,
//! connects each server (stdio or streamable HTTP) on the tokio runtime,
//! runs `tools/list`, and wraps each tool in an `McpTool`. `global()` serves
//! the tool list to `tools::main_registry`.
//!
//! First version is synchronous at startup: `init` blocks until all servers
//! finish connecting (per-server 30s timeout). A failed server is warn-logged
//! and skipped — it never blocks the rest or crashes manox, since MCP is an
//! optional enhancement. Servers are not hot-reloaded.

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use gpui::App;
use serde::Deserialize;

use crate::mcp::config::{McpConfig, McpServerTransportConfig};
use crate::mcp::tool::{McpClientHandle, McpTool};
use crate::tool::AnyAgentTool;

static REGISTRY: OnceLock<McpRegistry> = OnceLock::new();

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

pub struct McpRegistry {
    tools: Vec<AnyAgentTool>,
}

impl McpRegistry {
    pub fn tools(&self) -> &[AnyAgentTool] {
        &self.tools
    }
}

/// Read the config, connect every server, list tools. Call at App startup,
/// after `runtime::init` and `provider::registry::init`.
pub fn init(_cx: &mut App) {
    let mut config = match crate::paths::manox_config_dir() {
        Ok(dir) => McpConfig::load(&dir).unwrap_or_else(|e| {
            tracing::warn!("Failed to load MCP config, skipped: {e:#}");
            McpConfig::default()
        }),
        Err(e) => {
            tracing::warn!("Cannot locate manox config dir, MCP disabled: {e:#}");
            McpConfig::default()
        }
    };
    // Layer plugin-declared servers on top of the user's mcp.toml. Each
    // plugin's `.mcp.json` contributes its `mcpServers` under a
    // `<plugin>__<server>` key so two plugins exposing same-named servers
    // cannot clobber each other (or a user-declared server in mcp.toml).
    merge_plugin_declarations(&mut config);

    let registry = build_registry(config);
    let count = registry.tools.len();
    if count > 0 {
        tracing::info!("MCP registry ready: {count} tools");
    } else {
        tracing::info!("MCP registry empty (no servers connected)");
    }
    if let Err(rejected) = REGISTRY.set(registry) {
        tracing::warn!(
            "MCP registry already initialized; new registry ({} tools) rejected",
            rejected.tools.len()
        );
    }
}

/// Merge every installed plugin's `.mcp.json` declarations into `config`.
///
/// The plugin file uses the Claude Code shape `{ "mcpServers": { <name>: <cfg> } }`
/// (camelCase key); each entry deserializes straight into `McpServerConfig`,
/// which supports both stdio and streamable-HTTP transports. The server key
/// becomes `<plugin>__<server>` so plugin servers never collide with each
/// other or with user-declared `mcp.toml` entries (the user's `mcp.toml` wins
/// on an exact key clash by being inserted first; plugin merge only fills
/// absent keys — see the `entry().or_insert` below).
fn merge_plugin_declarations(config: &mut McpConfig) {
    for plugin in crate::plugin::PluginManager::installed() {
        let path = plugin.root.join(".mcp.json");
        let raw = match std::fs::read_to_string(&path) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                tracing::warn!("reading {}: {e}", path.display());
                continue;
            }
        };
        let parsed: PluginMcpFile = match serde_json::from_str(&raw) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!("parsing {}: {e}", path.display());
                continue;
            }
        };
        for (server_name, cfg) in parsed.mcp_servers {
            let key = format!("{}__{}", plugin.name, server_name);
            config.mcp_servers.entry(key).or_insert(cfg);
        }
    }
}

/// A plugin's `.mcp.json`: a `mcpServers` map (camelCase, matching Claude Code's
/// `.mcp.json` convention) of server configs. Reuses `McpServerConfig` so
/// plugins get the same stdio/HTTP transport support as `mcp.toml`.
#[derive(Debug, Deserialize)]
struct PluginMcpFile {
    #[serde(default, alias = "mcpServers")]
    mcp_servers: BTreeMap<String, crate::mcp::config::McpServerConfig>,
}

fn build_registry(config: McpConfig) -> McpRegistry {
    if config.mcp_servers.is_empty() {
        return McpRegistry { tools: Vec::new() };
    }
    let handle = crate::runtime::handle();
    // Block on connecting all servers. The tokio runtime is multi-threaded and
    // lives for the process; init runs on the gpui main thread before any UI.
    // `handle.spawn` (not bare `tokio::spawn`) makes the runtime handle
    // explicit — we are on the gpui main thread, not inside a tokio worker.
    let tools = handle.block_on(async { connect_all(handle.clone(), config.mcp_servers).await });
    McpRegistry { tools }
}

/// Connect every server concurrently, collect the resulting tool adapters.
/// Per-server failures are isolated.
async fn connect_all(
    handle: tokio::runtime::Handle,
    servers: BTreeMap<String, crate::mcp::config::McpServerConfig>,
) -> Vec<AnyAgentTool> {
    let mut tasks = Vec::new();
    for (name, cfg) in servers {
        tasks.push(handle.spawn(async move { connect_one(&name, cfg).await }));
    }
    let mut all_tools = Vec::new();
    for task in tasks {
        match task.await {
            Ok(Ok(tools)) => all_tools.extend(tools),
            Ok(Err(e)) => tracing::warn!("MCP server connection failed: {e:#}"),
            Err(e) => tracing::warn!("MCP server task panicked: {e}"),
        }
    }
    all_tools
}

/// Connect a single server, list its tools, wrap each as `McpTool`.
async fn connect_one(
    name: &str,
    cfg: crate::mcp::config::McpServerConfig,
) -> anyhow::Result<Vec<AnyAgentTool>> {
    let client = tokio::time::timeout(CONNECT_TIMEOUT, connect_transport(&cfg.transport))
        .await
        .map_err(|_| {
            anyhow::anyhow!("MCP server `{name}` connect timed out after {CONNECT_TIMEOUT:?}")
        })??;

    let tools = client
        .peer()
        .list_all_tools()
        .await
        .map_err(|e| anyhow::anyhow!("MCP server `{name}` tools/list failed: {e}"))?;

    tracing::info!("MCP server `{name}` exposed {} tools", tools.len());
    let handle: McpClientHandle = Arc::new(client);
    let wrapped = tools
        .into_iter()
        .map(|tool| Arc::new(McpTool::new(name.to_string(), tool, handle.clone())) as AnyAgentTool)
        .collect();
    Ok(wrapped)
}

/// Build a transport, run the rmcp client handshake, return the running service.
async fn connect_transport(
    transport: &McpServerTransportConfig,
) -> anyhow::Result<rmcp::service::RunningService<rmcp::service::RoleClient, rmcp::model::ClientInfo>>
{
    let client_info = rmcp::model::ClientInfo::default();
    let service = match transport {
        McpServerTransportConfig::Stdio { command, args, env, cwd } => {
            let mut cmd = tokio::process::Command::new(command);
            cmd.args(args).kill_on_drop(true);
            if let Some(env) = env {
                for (k, v) in env {
                    cmd.env(k, v);
                }
            }
            if let Some(cwd) = cwd {
                cmd.current_dir(cwd);
            }
            // `TokioChildProcess` takes the un-spawned `Command` and spawns it
            // itself; its builder defaults to piped stdin/stdout and inherited
            // stderr, which is what MCP JSON-RPC over stdio needs.
            let (child, _stderr) =
                rmcp::transport::child_process::TokioChildProcess::builder(cmd)
                    .spawn()
                    .map_err(|e| anyhow::anyhow!("spawning MCP stdio server `{command}`: {e}"))?;
            rmcp::service::serve_client(client_info, child).await
        }
        McpServerTransportConfig::StreamableHttp { url, headers } => {
            let mut config =
                rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig::with_uri(url.as_str());
            if let Some(headers) = headers {
                config = config.custom_headers(header_map(headers)?);
            }
            // `from_config` uses rmcp's own reqwest client (matching its
            // `StreamableHttpClient` impl); reusing manox's reqwest 0.12 client
            // here would not satisfy the trait bound.
            let transport =
                rmcp::transport::streamable_http_client::StreamableHttpClientTransport::from_config(config);
            rmcp::service::serve_client(client_info, transport).await
        }
    }
    .map_err(|e| anyhow::anyhow!("MCP client initialize failed: {e}"))?;
    Ok(service)
}

/// Returns the global registry. Panics if `init` was not called.
pub fn global() -> &'static McpRegistry {
    REGISTRY
        .get()
        .expect("McpRegistry not initialized; call agent::init first")
}

/// Non-panicking accessor for callers that may run before `agent::init`
/// (e.g. unit tests building a `ToolRegistry` directly). Returns `None`
/// until `init` has populated the registry.
pub fn try_global() -> Option<&'static McpRegistry> {
    REGISTRY.get()
}

/// Build a `HashMap<HeaderName, HeaderValue>` from the config's string-string
/// header table for the streamable-HTTP transport.
fn header_map(
    headers: &BTreeMap<String, String>,
) -> anyhow::Result<std::collections::HashMap<http::HeaderName, http::HeaderValue>> {
    let mut map = std::collections::HashMap::new();
    for (k, v) in headers {
        let name = http::HeaderName::from_bytes(k.as_bytes())
            .map_err(|e| anyhow::anyhow!("invalid header name `{k}`: {e}"))?;
        let val = http::HeaderValue::from_str(v)
            .map_err(|e| anyhow::anyhow!("invalid header value for `{k}`: {e}"))?;
        map.insert(name, val);
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::config::{McpConfig, McpServerConfig, McpServerTransportConfig};

    #[test]
    fn plugin_mcp_file_parses_camel_case_key() {
        // A plugin's .mcp.json uses the Claude Code shape (camelCase mcpServers)
        // and the same per-entry transport shape as mcp.toml.
        let raw = r#"{
            "mcpServers": {
                "fs": { "command": "npx", "args": ["-y", "fs-server"] },
                "remote": { "url": "https://mcp.example.com/sse" }
            }
        }"#;
        let parsed: PluginMcpFile = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.mcp_servers.len(), 2);
        assert!(matches!(
            parsed.mcp_servers["fs"].transport,
            McpServerTransportConfig::Stdio { .. }
        ));
        assert!(matches!(
            parsed.mcp_servers["remote"].transport,
            McpServerTransportConfig::StreamableHttp { .. }
        ));
    }

    #[test]
    fn merge_plugin_declarations_namespaces_and_does_not_clobber_user() {
        // User mcp.toml declares `fs`; plugin `gitwork` also declares `fs`.
        // The merge must keep the user's entry and add the plugin's under a
        // namespaced key, so neither is lost.
        let mut config = McpConfig::default();
        config.mcp_servers.insert(
            "fs".to_string(),
            McpServerConfig {
                transport: McpServerTransportConfig::Stdio {
                    command: "user-binary".to_string(),
                    args: vec![],
                    env: None,
                    cwd: None,
                },
            },
        );
        // Simulate the merge step directly (PluginManager::installed reads the
        // filesystem, so we test the insertion policy in isolation).
        let mut plugin_entry = BTreeMap::new();
        plugin_entry.insert(
            "fs".to_string(),
            McpServerConfig {
                transport: McpServerTransportConfig::Stdio {
                    command: "npx".to_string(),
                    args: vec!["-y".to_string(), "fs-server".to_string()],
                    env: None,
                    cwd: None,
                },
            },
        );
        for (server_name, cfg) in plugin_entry {
            let key = format!("gitwork__{server_name}");
            config.mcp_servers.entry(key).or_insert(cfg);
        }
        // User's `fs` survives untouched.
        match &config.mcp_servers["fs"].transport {
            McpServerTransportConfig::Stdio { command, .. } => {
                assert_eq!(command, "user-binary");
            }
            _ => panic!("expected stdio"),
        }
        // Plugin's `fs` landed under the namespaced key.
        assert!(config.mcp_servers.contains_key("gitwork__fs"));
        match &config.mcp_servers["gitwork__fs"].transport {
            McpServerTransportConfig::Stdio { command, .. } => {
                assert_eq!(command, "npx");
            }
            _ => panic!("expected stdio"),
        }
    }
}
