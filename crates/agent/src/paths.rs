//! Filesystem paths for manox configuration.
//!
//! All manox-specific config lives under `~/.config/cx/manox/` (the `cx` config
//! root is shared with `cx.providers.config.yaml`): the SQLite database, agent
//! definitions, and any future state.

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

/// `$HOME/.config/cx/manox/skills` — user-authored skills (`<name>/SKILL.md`).
/// Plugin skills live under each plugin's `skills/` subdir instead.
pub fn skills_dir() -> Result<PathBuf> {
    Ok(manox_config_dir()?.join("skills"))
}

/// `$HOME/.config/cx/manox/commands` — user-authored slash commands (`<name>.md`).
/// Plugin commands live under each plugin's `commands/` subdir.
pub fn commands_dir() -> Result<PathBuf> {
    Ok(manox_config_dir()?.join("commands"))
}

/// `$HOME/.config/cx/manox/plugins` — installed plugin roots, one
/// subdirectory per plugin (`plugins/<name>/`). Populated by the plugin
/// manager on `install`; scanned by the skill/command/agent/hook loaders.
pub fn plugins_dir() -> Result<PathBuf> {
    Ok(manox_config_dir()?.join("plugins"))
}

/// Root directory of a single installed plugin.
pub fn plugin_root(name: &str) -> Result<PathBuf> {
    Ok(plugins_dir()?.join(name))
}

/// `$HOME/.config/cx/manox/marketplaces` — cloned marketplace git repos,
/// one per remote URL. Each clone contains a `.claude-plugin/marketplace.json`
/// index plus the `plugins/<name>/` sources the index points at.
pub fn marketplace_cache_dir() -> Result<PathBuf> {
    Ok(manox_config_dir()?.join("marketplaces"))
}

/// Stable filesystem-safe slug for a marketplace git URL: the last non-empty
/// path segment with a trailing `.git` stripped. Two URLs that resolve to the
/// same slug share a cache entry — mirroring Claude Code, which keys
/// marketplaces by name rather than by full URL. A trailing slash is tolerated
/// (the segment before it is used) so `…/x/` and `…/x` collide, as intended.
pub fn marketplace_slug(git_url: &str) -> String {
    let trimmed = git_url.trim_end_matches('/');
    let tail = trimmed.rsplit('/').next().unwrap_or(trimmed);
    tail.trim_end_matches(".git").to_string()
}

/// Directory holding the cloned marketplace repo for `git_url`.
pub fn marketplace_dir(git_url: &str) -> Result<PathBuf> {
    Ok(marketplace_cache_dir()?.join(marketplace_slug(git_url)))
}

/// File recording which plugins are currently enabled, one plugin name per line.
/// The loaders consult this to decide which `plugins/<name>/` roots to scan.
pub fn enabled_plugins_file() -> Result<PathBuf> {
    Ok(manox_config_dir()?.join("enabled_plugins.txt"))
}

/// `$HOME/.config/cx/manox/settings.toml` — plain-file user preferences (UI
/// language, …). Read once at startup by [`crate::settings`]; absence is normal
/// on a fresh machine and yields defaults.
pub fn settings_file() -> Result<PathBuf> {
    Ok(manox_config_dir()?.join("settings.toml"))
}

fn dirs() -> PathBuf {
    if let Some(p) = home_dir() {
        return p;
    }
    // No HOME env var: fall back to the process CWD so a missing HOME surfaces
    // as a benign relative path rather than a hard crash. Warn once so the
    // user notices (db/agents would otherwise silently land under CWD).
    tracing::warn!("HOME env var unset; manox config will live under the process CWD");
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

/// Ensure the manox config root exists. Called by writers (plugin manager,
/// sample-definition seeding) before they lay down files; readers tolerate
/// absence so a fresh machine with no config still boots.
pub fn ensure_manox_config_dir() -> Result<PathBuf> {
    let dir = manox_config_dir()?;
    if dir.exists() {
        return Ok(dir);
    }
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create manox config dir at {}", dir.display()))?;
    Ok(dir)
}
