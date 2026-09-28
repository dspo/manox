//! MCP (Model Context Protocol) client integration — shared core.
//!
//! Reads `~/.manox/mcp.toml` (layered with each installed plugin's
//! `.mcp.json`), connects every configured server (stdio or streamable HTTP)
//! via the `rmcp` SDK, and lists their tools. The harness bridges differ:
//! the manox harness wraps each tool as a manox `AgentTool` ([`napi_tool::PiMcpTool`]);
//! the retired manox harness wraps the same connected servers into its own
//! tool type. Configuration is file-only (no UI writes) in both.
//!
//! `init` blocks until all servers finish connecting (per-server timeout);
//! MCP is an optional enhancement and never blocks the rest of startup.
//! A failed server is **tracked**, not dropped: every configured server keeps
//! a slot with its lifecycle ([`ServerState`]), so the AHP face can render
//! `error`/`stopped` entries instead of pretending the server does not exist.
//! [`start`] / [`stop`] drive a slot across lifecycles at runtime (restart =
//! cancel the live client, reconnect from the stored config), and every
//! terminal transition is broadcast on [`subscribe_events`].

pub mod config;
pub mod napi_tool;

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use parking_lot::RwLock;
use rmcp::model::CallToolResult;
use rmcp::service::{RoleClient, RunningService};
use tokio::sync::broadcast;

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

/// Lifecycle of one configured MCP server, as the registry sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerState {
    /// Connected and serving tools.
    Ready,
    /// Last connect attempt failed; the config is kept for a later `start`.
    Error(String),
    /// Deliberately stopped (`stop`); the config is kept for a later `start`.
    Stopped,
}

/// Point-in-time identity + lifecycle of one configured server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotView {
    pub name: String,
    /// `file://` URI of the file that declares this server (mcp.toml or a
    /// plugin's `.mcp.json`) — the AHP customization's source URI.
    pub source_uri: String,
    pub state: ServerState,
}

/// A slot moved to a new terminal lifecycle state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerEvent {
    pub name: String,
    pub state: ServerState,
}

struct ServerSlot {
    name: String,
    source_uri: String,
    config: McpServerConfig,
    state: ServerState,
    client: Option<McpClientHandle>,
    tools: Vec<rmcp::model::Tool>,
    /// Bumped by every [`McpRegistry::begin_start`] and every stop. A connect
    /// that lands under a stale generation is discarded (its client
    /// cancelled) rather than applied — that is what keeps a stop from being
    /// silently overwritten by an in-flight start, and two overlapping
    /// starts from leaving an orphan client behind.
    generation: u64,
}

impl ServerSlot {
    fn view(&self) -> SlotView {
        SlotView {
            name: self.name.clone(),
            source_uri: self.source_uri.clone(),
            state: self.state.clone(),
        }
    }

    fn connected(&self) -> Option<ConnectedServer> {
        let client = self.client.as_ref()?;
        Some(ConnectedServer {
            name: self.name.clone(),
            client: Arc::clone(client),
            tools: self.tools.clone(),
        })
    }
}

/// Process-global MCP registry: one slot per configured server (clients of
/// ready servers kept alive) plus the event stream for lifecycle changes.
pub struct McpRegistry {
    slots: RwLock<Vec<ServerSlot>>,
}

impl McpRegistry {
    /// The ready servers — the tool-bridging view the engine builds tools
    /// from.
    pub fn servers(&self) -> Vec<ConnectedServer> {
        self.slots
            .read()
            .iter()
            .filter_map(ServerSlot::connected)
            .collect()
    }

    /// Every configured server with its lifecycle, in name (lexicographic)
    /// order — the config layers merge into a `BTreeMap`, so that *is* the
    /// config's canonical order.
    pub fn slots(&self) -> Vec<SlotView> {
        self.slots.read().iter().map(ServerSlot::view).collect()
    }

    /// One server's lifecycle view, by config key.
    pub fn slot(&self, name: &str) -> Option<SlotView> {
        self.slots
            .read()
            .iter()
            .find(|slot| slot.name == name)
            .map(ServerSlot::view)
    }

    pub fn tool_count(&self) -> usize {
        self.slots.read().iter().map(|slot| slot.tools.len()).sum()
    }

    fn config_of(&self, name: &str) -> Option<McpServerConfig> {
        self.slots
            .read()
            .iter()
            .find(|slot| slot.name == name)
            .map(|slot| slot.config.clone())
    }

    /// Create a fresh `Stopped` slot for a server that has none — the
    /// enable path for a server the startup filter dropped. A duplicate
    /// insert is a no-op: the existing slot's config and lifecycle win.
    fn insert_slot(&self, name: &str, source_uri: String, config: McpServerConfig) {
        let mut slots = self.slots.write();
        if slots.iter().any(|slot| slot.name == name) {
            return;
        }
        slots.push(ServerSlot {
            name: name.to_string(),
            source_uri,
            config,
            state: ServerState::Stopped,
            client: None,
            tools: Vec::new(),
            generation: 0,
        });
    }

    /// Begin a start: refuse unknown names synchronously, cancel any live
    /// client (restart semantics), and bump the generation the eventual
    /// connect is validated against.
    fn begin_start(&self, name: &str) -> Option<u64> {
        let mut slots = self.slots.write();
        let slot = slots.iter_mut().find(|slot| slot.name == name)?;
        slot.generation += 1;
        if let Some(client) = slot.client.take() {
            client.cancellation_token().cancel();
        }
        slot.tools = Vec::new();
        Some(slot.generation)
    }

    /// Apply a settled lifecycle under the slot's current generation.
    ///
    /// A connect that lands after a newer start/stop bumped the generation is
    /// discarded — its client, when there is one, is cancelled so a superseded
    /// start never leaves an orphan process behind.
    fn settle(
        &self,
        name: &str,
        generation: u64,
        state: ServerState,
        client: Option<McpClientHandle>,
        tools: Vec<rmcp::model::Tool>,
    ) -> bool {
        let mut slots = self.slots.write();
        let Some(slot) = slots.iter_mut().find(|slot| slot.name == name) else {
            if let Some(client) = client {
                client.cancellation_token().cancel();
            }
            return false;
        };
        if slot.generation != generation {
            if let Some(client) = client {
                client.cancellation_token().cancel();
            }
            return false;
        }
        slot.state = state;
        slot.client = client;
        slot.tools = tools;
        true
    }

    fn mark_stopped(&self, name: &str) -> bool {
        let mut slots = self.slots.write();
        match slots.iter_mut().find(|slot| slot.name == name) {
            Some(slot) => {
                slot.generation += 1;
                if let Some(client) = slot.client.take() {
                    client.cancellation_token().cancel();
                }
                slot.tools = Vec::new();
                slot.state = ServerState::Stopped;
                true
            }
            None => false,
        }
    }
}

static REGISTRY: OnceLock<McpRegistry> = OnceLock::new();
static EVENTS: OnceLock<broadcast::Sender<McpServerEvent>> = OnceLock::new();

fn events() -> &'static broadcast::Sender<McpServerEvent> {
    EVENTS.get_or_init(|| broadcast::channel(64).0)
}

/// Subscribe to slot lifecycle transitions. Events carry the terminal state
/// (`Ready`/`Error`/`Stopped`); the optimistic `starting` edge of an AHP
/// start request is the reducer's, not the registry's.
pub fn subscribe_events() -> broadcast::Receiver<McpServerEvent> {
    events().subscribe()
}

fn fire(name: &str, state: ServerState) {
    let _ = events().send(McpServerEvent {
        name: name.to_string(),
        state,
    });
}

/// Read the config (mcp.toml + plugin `.mcp.json` layers), connect every
/// server, list tools. Call at startup after `runtime::init`. Blocks until
/// all connections settle (per-server timeout); failures become `Error`
/// slots, keeping the server visible to the AHP face and restartable via
/// [`start`].
pub fn init() {
    let dir = crate::paths::manox_config_dir();
    let config = match &dir {
        Ok(dir) => McpConfig::load(dir).unwrap_or_else(|e| {
            tracing::warn!("Failed to load MCP config, skipped: {e:#}");
            McpConfig::default()
        }),
        Err(e) => {
            tracing::warn!("Cannot locate manox config dir, MCP disabled: {e:#}");
            McpConfig::default()
        }
    };
    let registry = build_registry(resolved_config(
        config,
        dir.ok().as_deref(),
        &crate::settings::mcp_disabled(),
    ));
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
        .expect("McpRegistry not initialized; call manox_agent::init first")
}

/// Non-panicking accessor for callers that may run before `init`.
pub fn try_global() -> Option<&'static McpRegistry> {
    REGISTRY.get()
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

/// Merge every installed plugin's `.mcp.json` declarations into `config`.
///
/// The plugin file uses the Claude Code shape `{ "mcpServers": { <name>: <cfg> } }`
/// (camelCase key); each entry deserializes straight into `McpServerConfig`.
/// The server key becomes `<plugin>__<server>` so plugin servers never
/// collide with each other or with user-declared `mcp.toml` entries (the
/// user's `mcp.toml` wins on an exact key clash by being inserted first).
///
/// Layering parity with [`resolved_config`], which repeats this fold with one
/// extra dimension (the declaring file, published as the AHP customization's
/// source URI) — a change to the merge rules must land in both.
pub fn merge_plugin_declarations(config: &mut McpConfig) {
    for record in config::list_plugin_declared_servers() {
        let key = format!("{}__{}", record.plugin, record.name);
        config.mcp_servers.entry(key).or_insert(record.config);
    }
}

/// Connect a single server and list its tools.
pub async fn connect_one(name: &str, cfg: &McpServerConfig) -> anyhow::Result<ConnectedServer> {
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

/// Fold the config layers into one name → (config, source URI) map, dropping
/// servers the user disabled in settings. User `mcp.toml` entries win on an
/// exact key clash by being inserted first; each entry records the file that
/// declares it, which is the source URI the AHP face publishes.
///
/// Layering parity with [`merge_plugin_declarations`] (same merge rules, plus
/// the source-URI dimension). When `config_dir` is `None` the user layer has
/// nowhere to come from and contributes nothing — the AHP customization's
/// `uri` is a required field, so an entry without a resolvable source is
/// skipped rather than published with an empty URI.
fn resolved_config(
    config: McpConfig,
    config_dir: Option<&std::path::Path>,
    disabled: &[String],
) -> BTreeMap<String, (McpServerConfig, String)> {
    let mut resolved: BTreeMap<String, (McpServerConfig, String)> = BTreeMap::new();
    if let Some(dir) = config_dir {
        let toml_uri = format!("file://{}", dir.join("mcp.toml").display());
        for (name, cfg) in &config.mcp_servers {
            resolved.insert(name.clone(), (cfg.clone(), toml_uri.clone()));
        }
    }
    for record in config::list_plugin_declared_servers() {
        let key = format!("{}__{}", record.plugin, record.name);
        resolved
            .entry(key)
            .or_insert_with(|| (record.config, format!("file://{}", record.source.display())));
    }
    if !disabled.is_empty() {
        resolved.retain(|name, _| {
            let keep = !disabled.iter().any(|d| d == name);
            if !keep {
                tracing::info!("MCP server `{name}` disabled in settings, skipped");
            }
            keep
        });
    }
    resolved
}

/// Start (or restart) one configured server.
///
/// Existence is checked synchronously so a caller gets an immediate refusal
/// for an unknown name; the connect runs detached and its outcome lands in
/// the slot and the event stream (`Ready` or `Error`) — validated against
/// the generation taken here, so a stop or a newer start that lands first
/// wins and the late connect is discarded with its client cancelled.
///
/// A settled `Ready` does **not** remount tools into live sessions: the
/// engine assembles a session's tools at open/create, so a session that was
/// open through the lifecycle sees the change on its next open.
pub fn start(name: &str) -> Result<(), String> {
    let Some(registry) = try_global() else {
        return Err("MCP registry is not initialized".to_string());
    };
    // A server the startup filter dropped (settings-disabled) has no slot;
    // its config still lives in the merged layers, and enabling it creates
    // the slot here.
    if registry.slot(name).is_none() {
        let resolved = resolved_config(
            config::load_global(),
            crate::paths::manox_config_dir().ok().as_deref(),
            &[],
        );
        let Some((config, source_uri)) = resolved.get(name) else {
            return Err(format!("unknown MCP server: {name}"));
        };
        registry.insert_slot(name, source_uri.clone(), config.clone());
    }
    let Some(config) = registry.config_of(name) else {
        return Err(format!("unknown MCP server: {name}"));
    };
    let Some(generation) = registry.begin_start(name) else {
        return Err(format!("unknown MCP server: {name}"));
    };
    let name = name.to_string();
    crate::runtime::handle().spawn(async move {
        match connect_one(&name, &config).await {
            Ok(server) => {
                let tools = server.tools.len();
                let applied = try_global().is_some_and(|r| {
                    r.settle(
                        &name,
                        generation,
                        ServerState::Ready,
                        Some(server.client),
                        server.tools,
                    )
                });
                if applied {
                    tracing::info!("MCP server `{name}` started: {tools} tools");
                    fire(&name, ServerState::Ready);
                } else {
                    tracing::warn!(
                        "MCP server `{name}` connected but a newer start/stop superseded it; client dropped"
                    );
                }
            }
            Err(e) => {
                let message = format!("{e:#}");
                tracing::warn!("MCP server `{name}` failed to start: {message}");
                let applied = try_global().is_some_and(|r| {
                    r.settle(
                        &name,
                        generation,
                        ServerState::Error(message.clone()),
                        None,
                        Vec::new(),
                    )
                });
                if applied {
                    fire(&name, ServerState::Error(message));
                }
            }
        }
    });
    Ok(())
}

/// Stop one configured server: the live client is cancelled, the generation
/// bumps (so an in-flight start's connect is discarded when it lands), and
/// the slot lands in `Stopped` (config kept for a later [`start`]). Unknown
/// names are a synchronous refusal.
pub fn stop(name: &str) -> Result<(), String> {
    let Some(registry) = try_global() else {
        return Err("MCP registry is not initialized".to_string());
    };
    if !registry.mark_stopped(name) {
        return Err(format!("unknown MCP server: {name}"));
    }
    tracing::info!("MCP server `{name}` stopped");
    fire(name, ServerState::Stopped);
    Ok(())
}

/// Connect every configured server concurrently and build the slot list.
/// Per-server failures are isolated into `Error` slots; the declaring config
/// rides with every slot so a failed server stays restartable via [`start`].
fn build_registry(servers: BTreeMap<String, (McpServerConfig, String)>) -> McpRegistry {
    let slots = if servers.is_empty() {
        Vec::new()
    } else {
        let handle = crate::runtime::handle();
        // Block on connecting all servers. The tokio runtime is multi-threaded
        // and lives for the process; init runs on the gpui main thread before
        // any UI. `handle.block_on` (not bare `tokio::spawn`) makes the runtime
        // handle explicit — we are on the gpui main thread, not inside a tokio
        // worker.
        handle.block_on(async {
            let mut tasks = Vec::new();
            let mut tasks_identity = Vec::new();
            for (name, (cfg, source_uri)) in servers {
                // The identity rides outside the task too: a panicked task's
                // join handle yields nothing, and the fallback slot still owes
                // the server's real name and source — an `unknown` slot with an
                // empty URI would publish a customization the AHP face cannot
                // render.
                let identity = (name.clone(), source_uri.clone(), cfg.clone());
                tasks.push(handle.spawn(async move {
                    let result = connect_one(&name, &cfg).await;
                    (name, source_uri, cfg, result)
                }));
                tasks_identity.push(identity);
            }
            let mut slots = Vec::new();
            for (task, identity) in tasks.into_iter().zip(tasks_identity) {
                let (name, source_uri, config, result) = match task.await {
                    Ok(outcome) => outcome,
                    Err(e) => {
                        let (name, source_uri, config) = identity;
                        (
                            name,
                            source_uri,
                            config,
                            Err(anyhow::anyhow!("MCP server task panicked: {e}")),
                        )
                    }
                };
                match result {
                    Ok(server) => slots.push(ServerSlot {
                        state: ServerState::Ready,
                        client: Some(server.client),
                        tools: server.tools,
                        config,
                        name,
                        source_uri,
                        generation: 0,
                    }),
                    Err(e) => {
                        let message = format!("{e:#}");
                        tracing::warn!("MCP server connection failed: {message}");
                        slots.push(ServerSlot {
                            state: ServerState::Error(message),
                            client: None,
                            tools: Vec::new(),
                            config,
                            name,
                            source_uri,
                            generation: 0,
                        });
                    }
                }
            }
            slots
        })
    };
    McpRegistry {
        slots: RwLock::new(slots),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::{CallToolResult, Content, RawContent, RawTextContent};

    fn stdio_cfg() -> McpServerConfig {
        McpServerConfig {
            transport: McpServerTransportConfig::Stdio {
                command: "true".into(),
                args: vec![],
                env: None,
                cwd: None,
            },
        }
    }

    fn config_dir() -> Option<&'static std::path::Path> {
        Some(std::path::Path::new("/cfg"))
    }

    #[test]
    fn resolved_config_drops_disabled_servers_only() {
        let mut config = McpConfig::default();
        config.mcp_servers.insert("alpha".into(), stdio_cfg());
        config.mcp_servers.insert("beta".into(), stdio_cfg());
        config.mcp_servers.insert("gamma".into(), stdio_cfg());
        let resolved = resolved_config(config, config_dir(), &["beta".to_string()]);
        let names: Vec<&String> = resolved.keys().collect();
        assert_eq!(names, ["alpha", "gamma"]);
    }

    #[test]
    fn resolved_config_noop_on_empty_disabled_list() {
        let mut config = McpConfig::default();
        config.mcp_servers.insert("alpha".into(), stdio_cfg());
        assert_eq!(resolved_config(config, config_dir(), &[]).len(), 1);
    }

    #[test]
    fn resolved_config_user_entry_wins_over_plugin_and_keeps_its_source() {
        let mut config = McpConfig::default();
        config.mcp_servers.insert("alpha".into(), stdio_cfg());
        let mut resolved = resolved_config(config, config_dir(), &[]);
        // A plugin declaring the same key must not displace the user entry:
        // the source stays the user's mcp.toml, never a plugin manifest.
        resolved
            .entry("alpha".to_string())
            .or_insert_with(|| (stdio_cfg(), "file:///plugin/.mcp.json".into()));
        let (config, source) = &resolved["alpha"];
        assert!(
            source.ends_with("mcp.toml"),
            "a user entry's source is mcp.toml: {source}"
        );
        assert!(matches!(
            &config.transport,
            McpServerTransportConfig::Stdio { command, .. } if command == "true"
        ));
    }

    #[test]
    fn a_user_entry_without_a_resolvable_config_dir_is_skipped_not_published_uri_less() {
        let mut config = McpConfig::default();
        config.mcp_servers.insert("alpha".into(), stdio_cfg());
        // The AHP customization's `uri` is required; an entry with no
        // resolvable source must not become an empty-URI customization.
        assert!(resolved_config(config, None, &[]).is_empty());
    }

    #[test]
    fn registry_slot_lifecycle_transitions() {
        let registry = McpRegistry {
            slots: RwLock::new(vec![ServerSlot {
                name: "alpha".into(),
                source_uri: "file:///mcp.toml".into(),
                config: stdio_cfg(),
                state: ServerState::Error("boom".into()),
                client: None,
                tools: Vec::new(),
                generation: 0,
            }]),
        };
        assert_eq!(
            registry.slot("alpha").unwrap().state,
            ServerState::Error("boom".into())
        );
        // stop() on a slot with no live client still lands in Stopped.
        assert!(registry.mark_stopped("alpha"));
        assert_eq!(registry.slot("alpha").unwrap().state, ServerState::Stopped);
        assert!(!registry.mark_stopped("missing"));
        assert!(registry.slot("missing").is_none());
    }

    #[test]
    fn a_settled_start_under_a_stale_generation_is_discarded() {
        let registry = McpRegistry {
            slots: RwLock::new(vec![ServerSlot {
                name: "alpha".into(),
                source_uri: "file:///mcp.toml".into(),
                config: stdio_cfg(),
                state: ServerState::Stopped,
                client: None,
                tools: Vec::new(),
                generation: 1,
            }]),
        };
        // A connect from generation 0 landing after the slot moved on must
        // not flip it to Ready — a stop (or a newer start) owns the slot.
        assert!(!registry.settle("alpha", 0, ServerState::Ready, None, Vec::new()));
        assert_eq!(registry.slot("alpha").unwrap().state, ServerState::Stopped);
        assert!(registry.settle("alpha", 1, ServerState::Ready, None, Vec::new()));
        assert_eq!(registry.slot("alpha").unwrap().state, ServerState::Ready);
    }

    #[test]
    fn a_stop_invalidates_an_in_flight_start() {
        let registry = McpRegistry {
            slots: RwLock::new(vec![ServerSlot {
                name: "alpha".into(),
                source_uri: "file:///mcp.toml".into(),
                config: stdio_cfg(),
                state: ServerState::Stopped,
                client: None,
                tools: Vec::new(),
                generation: 0,
            }]),
        };
        let generation = registry.begin_start("alpha").unwrap();
        assert!(generation > 0);
        // The user stops while the connect is in flight; the late connect
        // lands under the pre-stop generation and is discarded.
        assert!(registry.mark_stopped("alpha"));
        assert!(!registry.settle("alpha", generation, ServerState::Ready, None, Vec::new()));
        assert_eq!(registry.slot("alpha").unwrap().state, ServerState::Stopped);
    }

    #[test]
    fn events_carry_terminal_transitions() {
        let mut rx = subscribe_events();
        fire("alpha", ServerState::Ready);
        let event = rx.try_recv().unwrap();
        assert_eq!(event.name, "alpha");
        assert_eq!(event.state, ServerState::Ready);
        assert!(rx.try_recv().is_err());
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
