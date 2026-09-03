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

/// Session directory name before the T1 rename (`pi-sessions` → `sessions`).
const LEGACY_SESSIONS_DIR: &str = "pi-sessions";

/// One-shot upgrade for installs that predate the `pi-sessions` → `sessions`
/// rename: moves every legacy transcript (plus the `subagents/` subtree) into
/// the new directory so history built before the rename stays visible.
/// Destination entries that already exist win; the legacy directory is
/// removed once fully drained. Idempotent — a missing legacy dir is a no-op.
/// Returns the number of entries moved (files, plus the `subagents` dir when
/// moved whole).
pub fn migrate_legacy_sessions_dir() -> u64 {
    let Ok(home) = manox_home() else {
        return 0;
    };
    migrate_legacy_sessions_dir_in(&home)
}

/// Body of [`migrate_legacy_sessions_dir`] against an explicit home (test
/// seam — keeps tests out of the developer's real `~/.manox`).
pub fn migrate_legacy_sessions_dir_in(home: &std::path::Path) -> u64 {
    let legacy = home.join(LEGACY_SESSIONS_DIR);
    if !legacy.is_dir() {
        return 0;
    }
    let target = home.join("sessions");
    if let Err(error) = std::fs::create_dir_all(&target) {
        tracing::warn!(
            dir = %target.display(),
            %error,
            "cannot create sessions dir; legacy pi-sessions migration skipped"
        );
        return 0;
    }
    let mut moved = 0u64;
    if let Ok(entries) = std::fs::read_dir(&legacy) {
        for entry in entries.flatten() {
            // The subagents subtree merges separately below (its destination
            // may already exist with newer entries).
            if entry.file_name() == "subagents" {
                continue;
            }
            moved += move_entry(&entry.path(), &target.join(entry.file_name()));
        }
    }
    let legacy_sub = legacy.join("subagents");
    if legacy_sub.is_dir() {
        let target_sub = target.join("subagents");
        if target_sub.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&legacy_sub) {
                for entry in entries.flatten() {
                    moved += move_entry(&entry.path(), &target_sub.join(entry.file_name()));
                }
            }
        } else {
            moved += move_entry(&legacy_sub, &target_sub);
        }
    }
    // Drop the drained shell; warn if something unexpected is left behind.
    match std::fs::remove_dir(&legacy) {
        Ok(()) => {}
        Err(error) => tracing::warn!(
            dir = %legacy.display(),
            %error,
            "legacy sessions dir not empty after migration; leaving it in place"
        ),
    }
    moved
}

/// Rename one entry to `dest`; returns 1 on success. Existing destinations
/// win (a post-rename write is newer than any legacy content); the obsolete
/// legacy copy is removed so the legacy dir still drains. Errors warn.
fn move_entry(from: &std::path::Path, dest: &std::path::Path) -> u64 {
    if dest.exists() {
        if from.is_file()
            && let Err(error) = std::fs::remove_file(from)
        {
            tracing::warn!(
                path = %from.display(),
                %error,
                "failed to drop obsolete legacy session copy"
            );
        }
        return 0;
    }
    match std::fs::rename(from, dest) {
        Ok(()) => 1,
        Err(error) => {
            tracing::warn!(
                from = %from.display(),
                to = %dest.display(),
                %error,
                "failed to migrate legacy session entry"
            );
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_home(suffix: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "manox-migrate-{suffix}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    /// Merge migration: legacy transcripts (and the `subagents` subtree)
    /// land in `sessions/`; a conflicting destination entry wins; the legacy
    /// dir disappears; a second run is a no-op.
    #[test]
    fn migrates_legacy_pi_sessions_into_sessions() {
        let home = scratch_home("merge");
        let legacy = home.join(LEGACY_SESSIONS_DIR);
        let sessions = home.join("sessions");
        std::fs::create_dir_all(legacy.join("subagents")).unwrap();
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(legacy.join("a.jsonl"), "a").unwrap();
        std::fs::write(legacy.join("a.meta.json"), "{}").unwrap();
        std::fs::write(legacy.join("subagents").join("s.jsonl"), "s").unwrap();
        // Post-rename content already in the destination (e.g. ext-agents IPC
        // pairs) must survive the merge.
        std::fs::write(sessions.join("ipc.sock"), "").unwrap();
        std::fs::write(sessions.join("a.jsonl"), "newer").unwrap();

        let moved = migrate_legacy_sessions_dir_in(&home);
        assert_eq!(
            moved, 2,
            "a.meta.json + subagents dir (a.jsonl skipped: dest wins)"
        );
        assert!(!legacy.exists(), "legacy dir removed once drained");
        assert_eq!(
            std::fs::read_to_string(sessions.join("a.jsonl")).unwrap(),
            "newer",
            "existing destination wins on conflict"
        );
        assert!(sessions.join("a.meta.json").exists());
        assert!(sessions.join("subagents").join("s.jsonl").exists());
        assert!(sessions.join("ipc.sock").exists());
        assert_eq!(migrate_legacy_sessions_dir_in(&home), 0, "idempotent");

        std::fs::remove_dir_all(&home).unwrap();
    }

    /// Fast path: no `sessions` dir yet — the `subagents` subtree moves as
    /// one directory rename, transcripts move file by file.
    #[test]
    fn renames_subagents_whole_when_target_missing() {
        let home = scratch_home("whole");
        let legacy = home.join(LEGACY_SESSIONS_DIR);
        std::fs::create_dir_all(legacy.join("subagents")).unwrap();
        std::fs::write(legacy.join("b.jsonl"), "b").unwrap();

        let moved = migrate_legacy_sessions_dir_in(&home);
        assert_eq!(moved, 2, "b.jsonl + subagents dir");
        assert!(home.join("sessions").join("b.jsonl").exists());
        assert!(home.join("sessions").join("subagents").is_dir());
        assert!(!legacy.exists());

        std::fs::remove_dir_all(&home).unwrap();
    }
}
