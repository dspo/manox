//! MCP (Model Context Protocol) client integration — shared core.
//!
//! Reads `~/.config/cx/manox/mcp.toml` (layered with each installed plugin's
//! `.mcp.json`), connects every configured server (stdio or streamable HTTP)
//! via the `rmcp` SDK, and lists their tools. The harness bridges differ:
//! the pi path wraps each tool as a pi `AgentTool` ([`pi_tool::PiMcpTool`]);
//! the retired manox harness wraps the same connected servers into its own
//! tool type. Configuration is file-only (no UI writes) in both.
//!
//! `init` blocks until all servers finish connecting (per-server timeout);
//! a failed server is warn-logged and skipped — MCP is an optional
//! enhancement and never blocks the rest of startup.

pub mod config;
pub mod pi_tool;

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rmcp::model::CallToolResult;
use rmcp::service::{RoleClient, RunningService};

use crate::mcp::config::{McpConfig, McpServerConfig, McpServerTransportConfig};

/// Clonable handle to a running rmcp client. `RunningService` is cheaply
/// clonable (it wraps an `Arc`), so every tool from one server shares it.
pub type McpClientHandle = Arc<RunningService<RoleClient, rmcp::model::ClientInfo>>;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// One connected MCP server: the live client plus its advertised tools.
pub struct ConnectedServer {
    pub name: String,
    pub client: McpClientHandle,
    pub tools: Vec<rmcp::model::Tool>,
}

/// Process-global MCP registry: the connected servers (clients kept alive)
/// plus the tool inventory for the active harness bridge.
pub struct McpRegistry {
    servers: Vec<ConnectedServer>,
}

impl McpRegistry {
    pub fn servers(&self) -> &[ConnectedServer] {
        &self.servers
    }

    pub fn tool_count(&self) -> usize {
        self.servers.iter().map(|s| s.tools.len()).sum()
    }
}

static REGISTRY: OnceLock<McpRegistry> = OnceLock::new();

/// Read the config (mcp.toml + plugin `.mcp.json` layers), connect every
/// server, list tools. Call at startup after `runtime::init`. Blocks until
/// all connections settle (per-server timeout); failures are isolated.
pub fn init() {
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
    merge_plugin_declarations(&mut config);
    retain_enabled(&mut config, &crate::settings::mcp_disabled());

    let registry = build_registry(config);
    let count = registry.tool_count();
    if count > 0 {
        tracing::info!("MCP registry ready: {count} tools");
    } else {
        tracing::info!("MCP registry empty (no servers connected)");
    }
    if let Err(rejected) = REGISTRY.set(registry) {
        tracing::warn!(
            "MCP registry already initialized; new registry ({} tools) rejected",
            rejected.tool_count()
        );
    }
}

/// Returns the global registry. Panics if `init` was not called.
pub fn global() -> &'static McpRegistry {
    REGISTRY
        .get()
        .expect("McpRegistry not initialized; call agent::init first")
}

/// The merged MCP config (mcp.toml + plugin `.mcp.json` layers) without
/// connecting — the settings panel lists configured servers from it.
pub fn load_merged_config() -> McpConfig {
    let mut config = crate::paths::manox_config_dir()
        .ok()
        .and_then(|dir| McpConfig::load(&dir).ok())
        .unwrap_or_default();
    merge_plugin_declarations(&mut config);
    config
}

/// Drop servers the user disabled in settings (persisted `[mcp] disabled`);
/// the registry is built once at startup, so the switch applies on the next
/// launch.
fn retain_enabled(config: &mut McpConfig, disabled: &[String]) {
    if disabled.is_empty() {
        return;
    }
    config.mcp_servers.retain(|name, _| {
        let keep = !disabled.iter().any(|d| d == name);
        if !keep {
            tracing::info!("MCP server `{name}` disabled in settings, skipped");
        }
        keep
    });
}

/// Non-panicking accessor for callers that may run before `init`.
pub fn try_global() -> Option<&'static McpRegistry> {
    REGISTRY.get()
}

fn build_registry(config: McpConfig) -> McpRegistry {
    if config.mcp_servers.is_empty() {
        return McpRegistry {
            servers: Vec::new(),
        };
    }
    let handle = crate::runtime::handle();
    // Block on connecting all servers. The tokio runtime is multi-threaded
    // and lives for the process; init runs on the gpui main thread before
    // any UI. `handle.block_on` (not bare `tokio::spawn`) makes the runtime
    // handle explicit — we are on the gpui main thread, not inside a tokio
    // worker.
    let servers = handle.block_on(async { connect_all(handle.clone(), config.mcp_servers).await });
    McpRegistry { servers }
}

/// Connect every server concurrently. Per-server failures are isolated.
pub async fn connect_all(
    handle: tokio::runtime::Handle,
    servers: BTreeMap<String, McpServerConfig>,
) -> Vec<ConnectedServer> {
    let mut tasks = Vec::new();
    for (name, cfg) in servers {
        tasks.push(handle.spawn(async move { connect_one(&name, cfg).await }));
    }
    let mut connected = Vec::new();
    for task in tasks {
        match task.await {
            Ok(Ok(server)) => connected.push(server),
            Ok(Err(e)) => tracing::warn!("MCP server connection failed: {e:#}"),
            Err(e) => tracing::warn!("MCP server task panicked: {e}"),
        }
    }
    connected
}

/// Connect a single server and list its tools.
pub async fn connect_one(name: &str, cfg: McpServerConfig) -> anyhow::Result<ConnectedServer> {
    let client = tokio::time::timeout(CONNECT_TIMEOUT, connect_transport(name, &cfg.transport))
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
    Ok(ConnectedServer {
        name: name.to_string(),
        client: Arc::new(client),
        tools,
    })
}

/// Build a transport, run the rmcp client handshake, return the running
/// service.
///
/// Stdio servers are spawned through the `supervisor` process bus so manox
/// owns the `Child` (own process group, reaped on exit via `terminate_all` /
/// `killpg`) and rmcp only consumes the piped stdin/stdout streams.
/// Streamable-HTTP servers are not subprocesses and skip the bus.
pub async fn connect_transport(
    name: &str,
    transport: &McpServerTransportConfig,
) -> anyhow::Result<RunningService<RoleClient, rmcp::model::ClientInfo>> {
    let client_info = rmcp::model::ClientInfo::default();
    let service = match transport {
        McpServerTransportConfig::Stdio {
            command,
            args,
            env,
            cwd,
        } => {
            let mut cmd = tokio::process::Command::new(command);
            cmd.args(args);
            if let Some(env) = env {
                for (k, v) in env {
                    cmd.env(k, v);
                }
            }
            if let Some(cwd) = cwd {
                cmd.current_dir(cwd);
            }
            let spawned = supervisor::global()
                .spawn(
                    &format!("mcp-{name}"),
                    cmd,
                    supervisor::ProcessKind::Mcp,
                )
                .await
                .map_err(|e| anyhow::anyhow!("spawning MCP stdio server `{command}`: {e}"))?;
            let transport = rmcp::transport::IntoTransport::into_transport((
                spawned.stdout,
                spawned.stdin,
            ));
            rmcp::service::serve_client(client_info, transport).await
        }
        McpServerTransportConfig::StreamableHttp { url, headers } => {
            let mut config =
                rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig::with_uri(
                    url.as_str(),
                );
            if let Some(headers) = headers {
                config = config.custom_headers(header_map(headers)?);
            }
            let transport =
                rmcp::transport::streamable_http_client::StreamableHttpClientTransport::from_config(
                    config,
                );
            rmcp::service::serve_client(client_info, transport).await
        }
    }
    .map_err(|e| anyhow::anyhow!("MCP client initialize failed: {e}"))?;
    Ok(service)
}

/// Merge every installed plugin's `.mcp.json` declarations into `config`.
///
/// The plugin file uses the Claude Code shape `{ "mcpServers": { <name>: <cfg> } }`
/// (camelCase key); each entry deserializes straight into `McpServerConfig`.
/// The server key becomes `<plugin>__<server>` so plugin servers never
/// collide with each other or with user-declared `mcp.toml` entries (the
/// user's `mcp.toml` wins on an exact key clash by being inserted first).
pub fn merge_plugin_declarations(config: &mut McpConfig) {
    for record in config::list_plugin_declared_servers() {
        let key = format!("{}__{}", record.plugin, record.name);
        config.mcp_servers.entry(key).or_insert(record.config);
    }
}

/// Concatenated text content from an MCP tool result. Non-text blocks
/// (image/audio/resource) are skipped with a warn.
pub fn flatten_call_tool_result(result: &CallToolResult) -> McpToolOutput {
    let mut text = String::new();
    for content in &result.content {
        let raw: &rmcp::model::RawContent = &content.raw;
        match raw {
            rmcp::model::RawContent::Text(t) => {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&t.text);
            }
            rmcp::model::RawContent::Image(_) => {
                tracing::warn!("skipping MCP image content block in tool result");
            }
            rmcp::model::RawContent::Audio(_) => {
                tracing::warn!("skipping MCP audio content block in tool result");
            }
            rmcp::model::RawContent::Resource(_) | rmcp::model::RawContent::ResourceLink(_) => {
                tracing::warn!("skipping MCP resource content block in tool result");
            }
        }
    }
    McpToolOutput {
        text,
        is_error: result.is_error.unwrap_or(false),
    }
}

/// Flattened MCP tool result: the concatenated text plus the server's error
/// flag.
pub struct McpToolOutput {
    pub text: String,
    pub is_error: bool,
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
    use rmcp::model::{CallToolResult, Content, RawContent, RawTextContent};

    fn stdio_cfg() -> McpServerConfig {
        McpServerConfig {
            transport: crate::mcp::config::McpServerTransportConfig::Stdio {
                command: "true".into(),
                args: vec![],
                env: None,
                cwd: None,
            },
        }
    }

    #[test]
    fn retain_enabled_drops_disabled_servers_only() {
        let mut config = McpConfig::default();
        config.mcp_servers.insert("alpha".into(), stdio_cfg());
        config.mcp_servers.insert("beta".into(), stdio_cfg());
        config.mcp_servers.insert("gamma".into(), stdio_cfg());
        retain_enabled(&mut config, &["beta".to_string()]);
        let names: Vec<&String> = config.mcp_servers.keys().collect();
        assert_eq!(names.len(), 2);
        assert!(config.mcp_servers.contains_key("alpha"));
        assert!(config.mcp_servers.contains_key("gamma"));
    }

    #[test]
    fn retain_enabled_noop_on_empty_disabled_list() {
        let mut config = McpConfig::default();
        config.mcp_servers.insert("alpha".into(), stdio_cfg());
        retain_enabled(&mut config, &[]);
        assert_eq!(config.mcp_servers.len(), 1);
    }

    #[test]
    fn flatten_text_blocks() {
        let result = CallToolResult::success(vec![
            Content::new(
                RawContent::Text(RawTextContent {
                    text: "hello".into(),
                    meta: None,
                }),
                None,
            ),
            Content::new(
                RawContent::Text(RawTextContent {
                    text: "world".into(),
                    meta: None,
                }),
                None,
            ),
        ]);
        let out = flatten_call_tool_result(&result);
        assert_eq!(out.text, "hello\nworld");
        assert!(!out.is_error);
    }

    #[test]
    fn flatten_error_result() {
        let result = CallToolResult::error(vec![Content::new(
            RawContent::Text(RawTextContent {
                text: "boom".into(),
                meta: None,
            }),
            None,
        )]);
        let out = flatten_call_tool_result(&result);
        assert!(out.is_error);
        assert_eq!(out.text, "boom");
    }
}
