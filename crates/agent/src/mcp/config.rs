//! MCP server configuration types — parsed from `~/.config/cx/manox/mcp.toml`.
//!
//! The file is a `[mcp_servers.<name>]` map. Each entry is either a stdio
//! command (`command` + `args`) or a streamable-HTTP endpoint (`url`). A
//! missing file is benign (no servers); a malformed file is warn-logged and
//! skipped so manox still starts.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context as _, Result};
use serde::Deserialize;

/// Top-level config: a map of server name → server config.
#[derive(Debug, Default, Deserialize)]
pub struct McpConfig {
    #[serde(default)]
    pub mcp_servers: BTreeMap<String, McpServerConfig>,
}

/// One MCP server entry. The transport is chosen by which field is present:
/// `command` → stdio, `url` → streamable HTTP.
#[derive(Debug, Clone, Deserialize)]
pub struct McpServerConfig {
    #[serde(flatten)]
    pub transport: McpServerTransportConfig,
}

#[derive(Debug, Clone, Deserialize)]
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

impl McpConfig {
    /// Read and parse `mcp.toml` from the manox config dir. Returns an empty
    /// config (no servers) when the file is absent. A parse failure is
    /// returned as an error so the caller can decide to warn-and-continue.
    pub fn load(dir: &Path) -> Result<Self> {
        let path = dir.join("mcp.toml");
        let raw = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::debug!("no mcp.toml at {}, MCP disabled", path.display());
                return Ok(Self::default());
            }
            Err(e) => {
                return Err(e).with_context(|| format!("reading {}", path.display()));
            }
        };
        let cfg =
            toml::from_str::<Self>(&raw).with_context(|| format!("parsing {}", path.display()))?;
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_yields_no_servers() {
        let cfg: McpConfig = toml::from_str("").unwrap();
        assert!(cfg.mcp_servers.is_empty());
    }

    #[test]
    fn parses_stdio_and_http() {
        let toml = r#"
[mcp_servers.fs]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
env = { FOO = "bar" }

[mcp_servers.remote]
url = "https://mcp.example.com/sse"
[mcp_servers.remote.headers]
Authorization = "Bearer xxx"
"#;
        let cfg: McpConfig = toml::from_str(toml).unwrap();
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

    #[test]
    fn missing_file_is_empty() {
        // A directory that exists but contains no mcp.toml → empty config.
        let dir = std::env::temp_dir();
        let sub = dir.join("manox-mcp-test-missing");
        let _ = std::fs::remove_dir_all(&sub);
        std::fs::create_dir_all(&sub).unwrap();
        let cfg = McpConfig::load(&sub).unwrap();
        assert!(cfg.mcp_servers.is_empty());
        let _ = std::fs::remove_dir_all(&sub);
    }
}
