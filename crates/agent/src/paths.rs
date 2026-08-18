//! Filesystem paths for manox persistent state.
//!
//! All manox state lives under `~/.manox/` — the single root shared with the
//! cx provider config (`~/.manox/cx.providers.config.yaml`) and the cx CLI
//! state (`~/.manox/cx.db`, `~/.manox/sessions/`): the SQLite database, agent
//! definitions, and any future state.

use std::path::PathBuf;

use anyhow::{Context as _, Result};

/// `$HOME/.manox` — the single root for all manox (and cx-family) persistent state.
pub fn manox_home() -> Result<PathBuf> {
    Ok(dirs().join(".manox"))
}

/// `$HOME/.manox` — manox-specific config and state root.
pub fn manox_config_dir() -> Result<PathBuf> {
    manox_home()
}

/// `$HOME/.manox/agents` — subagent definition markdown files.
pub fn agents_dir() -> Result<PathBuf> {
    Ok(manox_config_dir()?.join("agents"))
}

/// `$HOME/.manox/skills` — user-authored skills (`<name>/SKILL.md`).
/// Plugin skills live under each plugin's `skills/` subdir instead.
pub fn skills_dir() -> Result<PathBuf> {
    Ok(manox_config_dir()?.join("skills"))
}

/// `$HOME/.manox/commands` — user-authored slash commands (`<name>.md`).
/// Plugin commands live under each plugin's `commands/` subdir.
pub fn commands_dir() -> Result<PathBuf> {
    Ok(manox_config_dir()?.join("commands"))
}

/// `$HOME/.manox/plugins` — installed plugin roots, one
/// subdirectory per plugin (`plugins/<name>/`). Populated by the plugin
/// manager on `install`; scanned by the skill/command/agent/hook loaders.
pub fn plugins_dir() -> Result<PathBuf> {
    Ok(manox_config_dir()?.join("plugins"))
}

/// Root directory of a single installed plugin.
pub fn plugin_root(name: &str) -> Result<PathBuf> {
    Ok(plugins_dir()?.join(name))
}

/// `$HOME/.manox/marketplaces` — cloned marketplace git repos,
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

/// File recording installed plugins that are explicitly disabled, one plugin
/// name per line. A disabled plugin stays on disk (so it survives as
/// installed) but is excluded from the loader-scanned set until re-enabled.
pub fn disabled_plugins_file() -> Result<PathBuf> {
    Ok(manox_config_dir()?.join("disabled_plugins.txt"))
}

/// `$HOME/.manox/settings.toml` — plain-file user preferences (UI
/// language, …). Read once at startup by [`crate::settings`]; absence is normal
/// on a fresh machine and yields defaults.
pub fn settings_file() -> Result<PathBuf> {
    Ok(manox_config_dir()?.join("settings.toml"))
}

/// `$HOME/.manox/themes` — terminal color themes (`.ottytheme`
/// TOML files), referenced by name from `[terminal].theme` in settings.
pub fn themes_dir() -> Result<PathBuf> {
    Ok(manox_config_dir()?.join("themes"))
}

/// Global plan-file directory (`~/.manox/plans`). Plan mode writes one
/// `<slug>-plan.md` per planned task here — session-local planning artifacts
/// that stay out of every working tree (and its git status), readable by any
/// thread so an approved plan survives a fresh-context execution handoff.
pub fn plans_dir() -> Result<PathBuf> {
    Ok(dirs().join(".manox").join("plans"))
}

/// Ensure the plans directory exists, creating it (and parents) as needed.
pub fn ensure_plans_dir() -> Result<PathBuf> {
    let dir = plans_dir()?;
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn dirs() -> PathBuf {
    if let Some(p) = home_dir() {
        return p;
    }
    // No HOME env var: fall back to the process CWD so a missing HOME surfaces
    // as a benign relative path rather than a hard crash. Warn once so the
    // user notices (db/agents would otherwise silently land under CWD).
    tracing::warn!("HOME env var unset; manox state will live under the process CWD");
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

pub fn home_dir() -> Option<PathBuf> {
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
