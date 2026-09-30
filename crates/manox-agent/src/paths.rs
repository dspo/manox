//! Filesystem paths for manox persistent state and the shared Claude Code
//! ecosystem home.
//!
//! manox splits its on-disk footprint in two:
//!
//! - `~/.manox/` — runtime state manox owns outright: the SQLite database,
//!   session journals, provider config (`cx.providers.config.yaml`), settings,
//!   and the cx CLI state (`cx.db`, `sessions/`).
//! - `~/.claude/` — the Claude Code ecosystem home, shared in place with the
//!   Claude Code CLI: user-authored skills, commands, agents, rules, and the
//!   plugin/marketplace store. manox consumes and (via the plugin manager)
//!   maintains these surfaces directly, so an ecosystem asset installed once
//!   is visible to both tools.

use std::path::PathBuf;

use anyhow::{Context as _, Result};

/// `$HOME/.manox` — the single root for all manox (and cx-family) runtime state.
///
/// `MANOX_HOME` overrides the root wholesale (env wins over `$HOME`). Used by
/// embedders and tests wanting an isolated store (provider config lookup,
/// threads.db, session journals all live under the root).
pub fn manox_home() -> Result<PathBuf> {
    if let Some(root) = std::env::var_os("MANOX_HOME").filter(|r| !r.is_empty()) {
        return Ok(PathBuf::from(root));
    }
    Ok(dirs().join(".manox"))
}

/// `$HOME/.manox` — manox-specific config and state root.
pub fn manox_config_dir() -> Result<PathBuf> {
    manox_home()
}

/// `$HOME/.claude` — the Claude Code ecosystem home, shared in place with the
/// Claude Code CLI. Ecosystem surfaces (skills, commands, agents, rules, the
/// plugin store) live here, not under `~/.manox`.
///
/// `MANOX_CLAUDE_HOME` overrides the root wholesale (env wins over `$HOME`),
/// mirroring `MANOX_HOME`; used by tests and embedders wanting isolation from
/// a real Claude Code installation.
pub fn claude_home() -> Result<PathBuf> {
    if let Some(root) = std::env::var_os("MANOX_CLAUDE_HOME").filter(|r| !r.is_empty()) {
        return Ok(PathBuf::from(root));
    }
    Ok(dirs().join(".claude"))
}

/// `$HOME/.claude/agents` — user-authored subagent definition markdown files.
pub fn agents_dir() -> Result<PathBuf> {
    Ok(claude_home()?.join("agents"))
}

/// `$HOME/.manox/sessions` — session journal transcripts: the repository
/// scan root and the canonical creation/materialization target, so an
/// on-disk identity probe and the eventual file can never disagree about
/// the path.
pub fn sessions_dir() -> Result<PathBuf> {
    Ok(manox_config_dir()?.join("sessions"))
}

/// `$HOME/.claude/skills` — user-authored skills (`<name>/SKILL.md`).
/// Plugin skills live under each plugin's `skills/` subdir instead.
pub fn skills_dir() -> Result<PathBuf> {
    Ok(claude_home()?.join("skills"))
}

/// `$HOME/.claude/commands` — user-authored slash commands (`<name>.md`).
/// Plugin commands live under each plugin's `commands/` subdir.
pub fn commands_dir() -> Result<PathBuf> {
    Ok(claude_home()?.join("commands"))
}

/// `$HOME/.claude/plugins` — installed plugin roots, one
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

/// Ensure the manox config root exists. Called by writers (plugin manager,
/// settings, MCP config) before they lay down files; readers tolerate
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

/// Exposed `pub(crate)` under `cfg(test)` so sibling modules' env-mutating
/// tests (the plugin manager's) serialize against these through the same
/// lock.
#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    /// Serializes tests that mutate `MANOX_CLAUDE_HOME`: env vars are
    /// process-global and cargo runs test threads in parallel.
    pub(crate) static CLAUDE_HOME_LOCK: Mutex<()> = Mutex::new(());

    /// Hold for the duration of any env-mutating assertion.
    pub(crate) fn claude_home_lock() -> MutexGuard<'static, ()> {
        CLAUDE_HOME_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The ecosystem dirs hang off `claude_home`, not `manox_home`: flipping
    /// them back under `~/.manox` would silently fork the shared Claude Code
    /// ecosystem again. Reads take the same lock as the mutating sibling —
    /// otherwise test A could read `claude_home()` before, and `skills_dir()`
    /// after, a concurrent setter flips the var, and the two halves of the
    /// assertion disagree.
    #[test]
    fn ecosystem_dirs_live_under_claude_home() {
        let _guard = claude_home_lock();
        let claude = claude_home().unwrap();
        assert_eq!(skills_dir().unwrap(), claude.join("skills"));
        assert_eq!(commands_dir().unwrap(), claude.join("commands"));
        assert_eq!(agents_dir().unwrap(), claude.join("agents"));
        // The runtime root stays under MANOX_HOME/HOME — the two roots must
        // never collapse into one.
        assert_ne!(sessions_dir().unwrap(), claude.join("sessions"));
    }

    /// `MANOX_CLAUDE_HOME` redirects the ecosystem root wholesale.
    #[test]
    fn claude_home_env_overrides_ecosystem_root() {
        let _guard = claude_home_lock();
        let claude = tempfile::tempdir().unwrap();
        // SAFETY: serialized by CLAUDE_HOME_LOCK; restored before returning.
        unsafe { std::env::set_var("MANOX_CLAUDE_HOME", claude.path()) };
        let skills = skills_dir().unwrap();
        unsafe { std::env::remove_var("MANOX_CLAUDE_HOME") };
        assert_eq!(skills, claude.path().join("skills"));
    }
}
