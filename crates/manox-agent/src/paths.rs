//! Filesystem paths for manox persistent state and the shared Claude Code
//! ecosystem home.
//!
//! manox splits its on-disk footprint in two:
//!
//! - `~/.manox/` — runtime state manox owns outright: the session core tree
//!   (`core/<project-slug>/<session-id>/`), provider config
//!   (`cx.providers.config.yaml`), settings, and the cx CLI state (`cx.db`,
//!   whose `sessions/` holds that CLI's IPC sockets and registry JSON).
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
/// embedders and tests wanting an isolated store (provider config lookup and
/// the session core tree both live under the root).
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

/// The v1.0.0 state root: every session lives in its own directory under a
/// project bucket (`<root>/<project-slug>/<session-id>/log.v1.jsonl`), the
/// immutable identity beside it (`header.json`). The legacy `sessions/`
/// flat tree is never read again — no import, no fallback.
pub fn core_root() -> Result<PathBuf> {
    // The store owns the sessions root (a test store points at its own
    // temp tree; the production store builds from this same function).
    if let Some(store_root) =
        crate::thread_store::try_global().map(|_| crate::thread_store::global_sessions_dir())
    {
        return Ok(store_root);
    }
    Ok(manox_config_dir()?.join("core"))
}

/// The bucket a conversation's journal is sharded into: its project's path,
/// or the single bucket every conversation with no project shares.
///
/// The bucket is a storage locality key, fixed when the session directory is
/// first created and never moved — belonging can change (attach, detach)
/// without the journal changing shard, which is what keeps a folder move from
/// stranding a conversation's history.
///
/// A conversation with no project is a *special project*, not an absence: it
/// has the same shape as any other (one row, an id, a folder in the sidebar),
/// and differs only in its initial-cwd policy. Its bucket is the fixed
/// [`UNFILED`] name rather than a slug of that policy's directory — the
/// directory is a working detail that must not scatter one conversation's
/// history across a bucket per temporary path.
pub fn conversation_bucket(project: Option<&std::path::Path>) -> String {
    match project {
        Some(path) => manox_harness::session::identity::project_slug(&path.to_string_lossy()),
        None => manox_harness::session::identity::project_slug(UNFILED),
    }
}

/// The bucket name the project-less special project owns.
pub const UNFILED: &str = "unfiled";

/// The initial working directory a conversation with no project runs in,
/// created on demand — a conversation that is never spoken to leaves nothing
/// behind. Derived from the conversation id, so a reopen reuses the same
/// directory instead of minting a sibling.
pub fn unfiled_workspace(id: &str) -> PathBuf {
    std::env::temp_dir().join(format!("steer-{id}"))
}

/// One session's directory under the core root (created lazily by the
/// engine's first append — a session that never runs leaves nothing).
pub fn core_session_dir(project: Option<&std::path::Path>, id: &str) -> Result<PathBuf> {
    Ok(core_root()?.join(conversation_bucket(project)).join(id))
}

/// The engine journal file inside a session's core directory.
pub fn core_session_file(project: Option<&std::path::Path>, id: &str) -> Result<PathBuf> {
    Ok(core_session_dir(project, id)?.join(manox_harness::session::identity::JOURNAL_FILE))
}

/// `(dir, exact-file)` for a session's core location: the per-session
/// directory the engine passes on (snapshots, notes checks) plus the
/// journal file the builder pins. `Err` only when the config dir is
/// unresolvable — callers fall back to the legacy flat shape rather than
/// refusing to spawn.
pub fn core_session_parts(
    project: Option<&std::path::Path>,
    id: &str,
) -> Result<(PathBuf, Option<PathBuf>)> {
    let dir = core_session_dir(project, id)?;
    let file = dir.join(manox_harness::session::identity::JOURNAL_FILE);
    Ok((dir, Some(file)))
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

/// `$HOME/.claude/plugins` — the Claude Code plugin store, shared in place:
/// `installed_plugins.json` (the install registry),
/// `known_marketplaces.json`, `marketplaces/<slug>/` clones, and
/// `cache/<marketplace>/<plugin>/<version>/` installed trees. Written only
/// by the plugin manager's explicit user operations; scanned by the
/// skill/command/agent/hook/MCP loaders on every read.
pub fn plugins_dir() -> Result<PathBuf> {
    Ok(claude_home()?.join("plugins"))
}

/// `$HOME/.claude/plugins/installed_plugins.json` — the shared install
/// registry (schema version 2).
pub fn installed_plugins_file() -> Result<PathBuf> {
    Ok(plugins_dir()?.join("installed_plugins.json"))
}

/// `$HOME/.claude/plugins/known_marketplaces.json` — the shared marketplace
/// registration list.
pub fn known_marketplaces_file() -> Result<PathBuf> {
    Ok(plugins_dir()?.join("known_marketplaces.json"))
}

/// `$HOME/.claude/settings.json` — Claude Code's own settings file; manox
/// touches only its `enabledPlugins` map and round-trips the rest.
pub fn claude_settings_file() -> Result<PathBuf> {
    Ok(claude_home()?.join("settings.json"))
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

/// Ensure the manox config root exists. Called by writers (settings, MCP
/// config) before they lay down files; readers tolerate absence so a fresh
/// machine with no config still boots. The plugin store no longer lives
/// here — it moved to the shared Claude home.
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
        assert_ne!(core_root().unwrap(), claude.join("sessions"));
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

#[cfg(test)]
mod bucket_tests {
    use super::*;

    /// A conversation with no project is the special project: it shards into
    /// one fixed bucket, never one per working directory. A conversation that
    /// is never spoken to has no directory at all, so the bucket has to be
    /// answerable before any turn has run.
    #[test]
    fn a_conversation_without_a_project_shards_into_one_bucket() {
        assert_eq!(conversation_bucket(None), "--unfiled--");
        assert_eq!(
            conversation_bucket(None),
            conversation_bucket(None),
            "the unfiled bucket is a constant, not a per-conversation path"
        );
    }

    /// A project-bound conversation shards by its project, not by any working
    /// directory it happens to run in: belonging and cwd are disjoint.
    #[test]
    fn a_project_bound_conversation_shards_by_its_project() {
        assert_eq!(
            conversation_bucket(Some(std::path::Path::new("/p/proj"))),
            "--p-proj--"
        );
        assert_ne!(
            conversation_bucket(Some(std::path::Path::new("/p/proj"))),
            conversation_bucket(None)
        );
    }

    /// The initial working directory of a project-less conversation is
    /// derived from the conversation id, so a reopen reuses the same one
    /// instead of minting a sibling — and computing it creates nothing.
    #[test]
    fn the_unfiled_workspace_is_derived_and_lazy() {
        let dir = unfiled_workspace("abc-123");
        assert_eq!(dir, unfiled_workspace("abc-123"));
        assert_ne!(dir, unfiled_workspace("other"));
        assert!(
            dir.to_string_lossy().ends_with("steer-abc-123"),
            "named after the conversation: {dir:?}"
        );
        assert!(!dir.exists(), "computing the path must not create it");
    }
}
