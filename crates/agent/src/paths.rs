//! Filesystem paths for manox configuration.
//!
//! All manox-specific config lives under `~/.config/cx/manox/` (the `cx` config
//! root is shared with `cx.providers.config.yaml`). The SQLite database still
//! lives under the legacy `~/.config/manox/` until the path-migration PR lands;
//! agent definitions — a new concern — go straight to the new root.

use std::path::PathBuf;

use anyhow::{Context as _, Result};

/// `$HOME/.config/cx` — shared root for cx-family config.
pub fn cx_config_dir() -> Result<PathBuf> {
    let home = dirs();
    Ok(home.join(".config").join("cx"))
}

/// `$HOME/.config/cx/manox` — manox-specific config root.
pub fn manox_config_dir() -> Result<PathBuf> {
    Ok(cx_config_dir()?.join("manox"))
}

/// `$HOME/.config/cx/manox/agents` — subagent definition markdown files.
pub fn agents_dir() -> Result<PathBuf> {
    Ok(manox_config_dir()?.join("agents"))
}

fn dirs() -> PathBuf {
    if let Some(p) = home_dir() {
        return p;
    }
    // No HOME env var: fall back to the process CWD so a missing HOME surfaces
    // as a benign relative path rather than a hard crash.
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

/// Ensure the agents directory exists, creating it (and parents) as needed.
/// Called lazily before writing sample definitions; readers tolerate absence.
pub fn ensure_agents_dir() -> Result<PathBuf> {
    let dir = agents_dir()?;
    if dir.exists() {
        return Ok(dir);
    }
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create agents dir at {}", dir.display()))?;
    Ok(dir)
}
