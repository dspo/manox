//! MCP server configuration types — parsed from `~/.manox/mcp.json`.
//!
//! The file uses the Claude Code `mcpServers` shape:
//! `{ "mcpServers": { <name>: <cfg> } }` — the same schema as a project
//! `.mcp.json` and a plugin's `.mcp.json`, so one server entry reads
//! identically in all three layers. Each entry is either a stdio command
//! (`command` + `args`) or a streamable-HTTP endpoint (`url`). A missing
//! file is benign (no servers); a malformed file is warn-logged and skipped
//! so manox still starts.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

/// Top-level config: a map of server name → server config. Serialized under
/// the Claude Code `mcpServers` key.
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
pub struct McpConfig {
    #[serde(default, rename = "mcpServers")]
    pub mcp_servers: BTreeMap<String, McpServerConfig>,
}

/// One MCP server entry. The transport is chosen by which field is present:
/// `command` → stdio, `url` → streamable HTTP.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct McpServerConfig {
    #[serde(flatten)]
    pub transport: McpServerTransportConfig,
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum McpServerTransportConfig {
    /// Launch a local process speaking JSON-RPC over stdin/stdout.
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: Option<BTreeMap<String, String>>,
        #[serde(default)]
        cwd: Option<String>,
    },
    /// Connect to a remote streamable-HTTP MCP endpoint.
    StreamableHttp {
        url: String,
        #[serde(default)]
        headers: Option<BTreeMap<String, String>>,
    },
}

/// Hand-rolled (not `#[serde(untagged)]` derive) so misconfigured entries
/// get an actionable message instead of serde's generic "data did not match
/// any variant": untagged also silently ignores a stray `url` next to
/// `command`, picking stdio and dropping the field.
impl<'de> Deserialize<'de> for McpServerTransportConfig {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Raw {
            #[serde(default)]
            command: Option<String>,
            #[serde(default)]
            args: Vec<String>,
            #[serde(default)]
            env: Option<BTreeMap<String, String>>,
            #[serde(default)]
            cwd: Option<String>,
            #[serde(default)]
            url: Option<String>,
            #[serde(default)]
            headers: Option<BTreeMap<String, String>>,
        }
        let raw = Raw::deserialize(deserializer)?;
        match (raw.command, raw.url) {
            (Some(command), None) => Ok(Self::Stdio {
                command,
                args: raw.args,
                env: raw.env,
                cwd: raw.cwd,
            }),
            (None, Some(url)) => Ok(Self::StreamableHttp {
                url,
                headers: raw.headers,
            }),
            (Some(_), Some(_)) => Err(serde::de::Error::custom(
                "set either `command` (stdio) or `url` (streamable HTTP), not both",
            )),
            (None, None) => Err(serde::de::Error::custom(
                "missing transport: set `command` (stdio) or `url` (streamable HTTP)",
            )),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PluginMcpServerRecord {
    pub plugin: String,
    pub name: String,
    pub source: PathBuf,
    pub config: McpServerConfig,
}

impl McpConfig {
    /// Read and parse `mcp.json` from the manox config dir. Returns an empty
    /// config (no servers) when the file is absent. A parse failure is
    /// returned as an error so the caller can decide to warn-and-continue.
    pub fn load(dir: &Path) -> Result<Self> {
        let path = dir.join("mcp.json");
        let raw = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::debug!("no mcp.json at {}, MCP disabled", path.display());
                return Ok(Self::default());
            }
            Err(e) => {
                return Err(e).with_context(|| format!("reading {}", path.display()));
            }
        };
        let cfg = serde_json::from_str::<Self>(&raw)
            .with_context(|| format!("parsing {}", path.display()))?;
        Ok(cfg)
    }

    /// Read the project-level `.mcp.json` (Claude Code's public project-scope
    /// MCP convention) from `cwd`. Returns `None` when the file is absent;
    /// a malformed file is warn-logged and treated as absent.
    pub fn load_project(cwd: &Path) -> Option<(PathBuf, Self)> {
        let path = cwd.join(".mcp.json");
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => {
                tracing::warn!("reading {}: {e}", path.display());
                return None;
            }
        };
        match serde_json::from_str::<Self>(&raw) {
            Ok(cfg) => Some((path, cfg)),
            Err(e) => {
                tracing::warn!("parsing {}: {e}", path.display());
                None
            }
        }
    }
}

/// Convenience wrapper for the settings overlay: load the MCP config from the
/// process-wide manox config dir. Returns an empty config (no servers) when
/// the dir path is unavailable or the file is malformed, matching the
/// startup `init` behavior — UI surfaces should not crash on a bad config.
pub fn load_global() -> McpConfig {
    match crate::paths::manox_config_dir() {
        Ok(dir) => McpConfig::load(&dir).unwrap_or_default(),
        Err(e) => {
            tracing::warn!("MCP config dir unavailable: {e:#}; MCP settings panel will be empty");
            McpConfig::default()
        }
    }
}

/// Persist the user's `mcp.json` using the same schema that `load_global`
/// reads. Plugin-declared servers are not written here; they continue to live
/// inside each installed plugin's `.mcp.json`.
pub fn save_global(config: &McpConfig) -> Result<()> {
    let dir = crate::paths::ensure_manox_config_dir()
        .context("ensuring manox config dir exists before writing mcp.json")?;
    let path = dir.join("mcp.json");
    let mut body = serde_json::to_string_pretty(config).context("serializing mcp.json")?;
    body.push('\n');
    std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Scan installed plugins for `.mcp.json` declarations. The returned records
/// are read-only from the UI's perspective; editing happens in each plugin's
/// source, not in `mcp.toml`.
pub fn list_plugin_declared_servers() -> Vec<PluginMcpServerRecord> {
    #[derive(Debug, Deserialize, Default)]
    struct PluginMcpFile {
        #[serde(default, alias = "mcpServers")]
        mcp_servers: BTreeMap<String, McpServerConfig>,
    }

    let mut out = Vec::new();
    for plugin in crate::plugin::PluginManager::installed() {
        let path = plugin.root.join(".mcp.json");
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                tracing::warn!("reading {}: {e}", path.display());
                continue;
            }
        };
        let parsed: PluginMcpFile = match serde_json::from_str(&raw) {
            Ok(parsed) => parsed,
            Err(e) => {
                tracing::warn!("parsing {}: {e}", path.display());
                continue;
            }
        };
        for (name, config) in parsed.mcp_servers {
            out.push(PluginMcpServerRecord {
                plugin: plugin.name.clone(),
                name,
                source: path.clone(),
                config,
            });
        }
    }
    out.sort_by(|a, b| a.plugin.cmp(&b.plugin).then_with(|| a.name.cmp(&b.name)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_yields_no_servers() {
        let cfg: McpConfig = serde_json::from_str("{}").unwrap();
        assert!(cfg.mcp_servers.is_empty());
    }

    #[test]
    fn parses_claude_code_shape_stdio_and_http() {
        let json = r#"{
            "mcpServers": {
                "fs": {
                    "command": "npx",
                    "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"],
                    "env": {"FOO": "bar"}
                },
                "remote": {
                    "url": "https://mcp.example.com/sse",
                    "headers": {"Authorization": "Bearer xxx"}
                }
            }
        }"#;
        let cfg: McpConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.mcp_servers.len(), 2);
        let fs = cfg.mcp_servers.get("fs").unwrap();
        match &fs.transport {
            McpServerTransportConfig::Stdio {
                command,
                args,
                env,
                cwd,
            } => {
                assert_eq!(command, "npx");
                assert_eq!(
                    args,
                    &["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
                );
                assert_eq!(env.as_ref().unwrap().get("FOO").unwrap(), "bar");
                assert!(cwd.is_none());
            }
            _ => panic!("expected stdio"),
        }
        let remote = cfg.mcp_servers.get("remote").unwrap();
        match &remote.transport {
            McpServerTransportConfig::StreamableHttp { url, headers } => {
                assert_eq!(url, "https://mcp.example.com/sse");
                assert_eq!(
                    headers.as_ref().unwrap().get("Authorization").unwrap(),
                    "Bearer xxx"
                );
            }
            _ => panic!("expected http"),
        }
    }

    /// The written form is the Claude Code wire shape: a top-level
    /// `mcpServers` object, and it round-trips through a fresh parse.
    #[test]
    fn save_shape_round_trips_through_claude_code_key() {
        let cfg: McpConfig =
            serde_json::from_str(r#"{"mcpServers":{"fs":{"command":"npx"}}}"#).unwrap();
        let body = serde_json::to_string(&cfg).unwrap();
        assert!(body.contains(r#""mcpServers""#), "{body}");
        assert!(!body.contains("mcp_servers"), "{body}");
        let reparsed: McpConfig = serde_json::from_str(&body).unwrap();
        assert_eq!(reparsed.mcp_servers.len(), 1);
    }

    #[test]
    fn rejects_command_and_url_together() {
        let err = serde_json::from_str::<McpConfig>(
            r#"{"mcpServers":{"bad":{"command":"npx","url":"https://mcp.example.com"}}}"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("not both"), "{err:#}");
    }

    #[test]
    fn rejects_missing_transport() {
        let err = serde_json::from_str::<McpConfig>(r#"{"mcpServers":{"bad":{"args":["x"]}}}"#)
            .unwrap_err();
        assert!(err.to_string().contains("missing transport"), "{err:#}");
    }

    #[test]
    fn missing_file_is_empty() {
        // A directory that exists but contains no mcp.json → empty config.
        let dir = std::env::temp_dir();
        let sub = dir.join("manox-mcp-test-missing");
        let _ = std::fs::remove_dir_all(&sub);
        std::fs::create_dir_all(&sub).unwrap();
        let cfg = McpConfig::load(&sub).unwrap();
        assert!(cfg.mcp_servers.is_empty());
        let _ = std::fs::remove_dir_all(&sub);
    }

    #[test]
    fn project_layer_reads_mcp_json_and_tolerates_absence() {
        let proj = tempfile::tempdir().unwrap();
        assert!(McpConfig::load_project(proj.path()).is_none());

        std::fs::write(
            proj.path().join(".mcp.json"),
            r#"{"mcpServers":{"proj-fs":{"command":"npx"}}}"#,
        )
        .unwrap();
        let (path, cfg) = McpConfig::load_project(proj.path()).unwrap();
        assert_eq!(path, proj.path().join(".mcp.json"));
        assert_eq!(cfg.mcp_servers.len(), 1);

        std::fs::write(proj.path().join(".mcp.json"), "{not json").unwrap();
        assert!(McpConfig::load_project(proj.path()).is_none());
    }
}
