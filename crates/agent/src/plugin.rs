//! Plugin manager for the Claude Code marketplace ecosystem.
//!
//! A marketplace is a git repo whose root carries `.claude-plugin/marketplace.json`
//! — an index of plugins, each pointing at a subdirectory via a relative `source`
//! path. `add_marketplace` clones the repo into [`paths::marketplace_cache_dir`]
//! (shelling out to system `git`, never a `git2` dependency — the project forbids
//! vendored/git-submodule deps and `git2` would drag libgit2 into the binary).
//! `install` copies a plugin's source tree into [`paths::plugins_dir`] so the
//! installed set is self-contained: a marketplace may be removed without orphaning
//! installed plugins, and loaders scan a single directory.
//!
//! The loaders (skills / commands / agents / hooks) consume
//! [`PluginManager::installed_roots`]; this module is the only writer of
//! `plugins_dir`, so there is no race over plugin contents at runtime.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result};
use serde::Deserialize;

use crate::paths;

/// Parsed `.claude-plugin/marketplace.json`.
#[derive(Debug, Clone, Deserialize)]
pub struct MarketplaceIndex {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub plugins: Vec<MarketplacePluginEntry>,
}

/// One entry in a marketplace index.
#[derive(Debug, Clone, Deserialize)]
pub struct MarketplacePluginEntry {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// Relative path to the plugin source tree within the marketplace repo
    /// (e.g. `./plugins/gitwork`). Resolved against the repo root.
    pub source: String,
}

/// Parsed plugin `plugin.json` — minimal metadata.
#[derive(Debug, Clone, Deserialize)]
pub struct PluginManifest {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
}

/// A plugin root that loaders should scan: the installed plugin's directory plus
/// the marketplace slug it was installed from (for namespacing and `plugin:`
/// qualified lookups).
#[derive(Debug, Clone)]
pub struct InstalledPlugin {
    pub name: String,
    pub root: PathBuf,
    pub marketplace: String,
}

#[derive(Debug, Clone)]
pub struct MarketplaceRecord {
    pub slug: String,
    pub git_url: Option<String>,
    pub root: PathBuf,
    pub name: String,
    pub description: Option<String>,
    pub plugin_count: usize,
}

#[derive(Debug, Clone)]
pub struct MarketplacePluginRecord {
    pub marketplace_slug: String,
    pub name: String,
    pub description: Option<String>,
    pub source: String,
    pub installed: bool,
}

#[derive(Debug, Clone)]
pub struct InstalledPluginRecord {
    pub name: String,
    pub marketplace: String,
    pub root: PathBuf,
    pub description: Option<String>,
    pub version: Option<String>,
}

/// Filesystem-backed plugin manager. State is stored under
/// `~/.config/cx/manox/` (cloned marketplaces, installed plugin trees, the
/// enabled-plugins list), so it survives across sessions without an in-process
/// registry slot.
pub struct PluginManager;

impl PluginManager {
    /// Clone (or fast-forward update) a marketplace repo and return its index.
    /// A second `add` for the same URL refreshes the existing clone rather than
    /// failing — mirroring Claude Code, which re-pulls on re-registration.
    pub fn add_marketplace(git_url: &str) -> Result<MarketplaceIndex> {
        let dir = paths::marketplace_dir(git_url)?;
        paths::ensure_manox_config_dir()?;
        if dir.join(".git").exists() {
            // Refresh in place: fetch, then hard-reset the working tree to the
            // fetched tip. `git fetch` alone updates remote refs but leaves the
            // on-disk files (which `load_marketplace_index` reads) stale; the
            // reset is what actually surfaces the latest marketplace index.
            let fetch = Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(["fetch", "--all"])
                .status()
                .with_context(|| format!("git fetch in {}", dir.display()))?;
            if !fetch.success() {
                tracing::warn!(
                    "git fetch failed for marketplace {} — using existing clone",
                    git_url
                );
            } else {
                let reset = Command::new("git")
                    .arg("-C")
                    .arg(&dir)
                    .args(["reset", "--hard", "FETCH_HEAD"])
                    .status()
                    .with_context(|| format!("git reset in {}", dir.display()))?;
                if !reset.success() {
                    tracing::warn!(
                        "git reset failed for marketplace {} — index may be stale",
                        git_url
                    );
                }
            }
        } else {
            std::fs::create_dir_all(dir.parent().unwrap_or(Path::new(".")))
                .context("creating marketplace cache dir")?;
            let status = Command::new("git")
                .args(["clone", "--depth", "1", git_url])
                .arg(&dir)
                .status()
                .with_context(|| format!("git clone {}", git_url))?;
            if !status.success() {
                anyhow::bail!("git clone failed: {} (exit {:?})", git_url, status.code());
            }
        }
        Self::load_marketplace_index(&dir)
    }

    /// Remove a cloned marketplace repo. Installed plugins copied from it stay
    /// installed — they are independent trees under `plugins_dir`.
    pub fn remove_marketplace(git_url: &str) -> Result<()> {
        let dir = paths::marketplace_dir(git_url)?;
        if dir.exists() {
            std::fs::remove_dir_all(&dir)
                .with_context(|| format!("removing marketplace {}", dir.display()))?;
        }
        Ok(())
    }

    /// Parse the marketplace index from a cloned repo root.
    pub fn load_marketplace_index(repo_root: &Path) -> Result<MarketplaceIndex> {
        let index_path = repo_root.join(".claude-plugin").join("marketplace.json");
        let raw = std::fs::read_to_string(&index_path)
            .with_context(|| format!("reading marketplace index {}", index_path.display()))?;
        let idx: MarketplaceIndex = serde_json::from_str(&raw)
            .with_context(|| format!("parsing marketplace index {}", index_path.display()))?;
        Ok(idx)
    }

    /// List marketplace slugs present in the cache.
    pub fn list_marketplaces() -> Vec<String> {
        let Ok(dir) = paths::marketplace_cache_dir() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                if entry.path().join(".git").exists()
                    && let Ok(name) = entry.file_name().into_string()
                {
                    out.push(name);
                }
            }
        }
        out
    }

    /// Rich view of cached marketplaces for UI surfaces. Reads the current
    /// on-disk index and best-effort origin URL from each clone.
    pub fn list_marketplace_records() -> Vec<MarketplaceRecord> {
        let Ok(dir) = paths::marketplace_cache_dir() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return out;
        };
        for entry in entries.flatten() {
            let root = entry.path();
            if !root.join(".git").exists() {
                continue;
            }
            let Ok(slug) = entry.file_name().into_string() else {
                continue;
            };
            let Ok(index) = Self::load_marketplace_index(&root) else {
                continue;
            };
            out.push(MarketplaceRecord {
                slug,
                git_url: git_remote_origin(&root),
                root,
                name: index.name,
                description: index.description,
                plugin_count: index.plugins.len(),
            });
        }
        out.sort_by(|a, b| a.slug.cmp(&b.slug));
        out
    }

    /// Refresh a cached marketplace clone by slug.
    pub fn refresh_marketplace(slug: &str) -> Result<MarketplaceIndex> {
        let dir = paths::marketplace_cache_dir()?.join(slug);
        refresh_marketplace_dir(&dir)?;
        Self::load_marketplace_index(&dir)
    }

    /// Remove a cached marketplace clone by slug.
    pub fn remove_marketplace_by_slug(slug: &str) -> Result<()> {
        let dir = paths::marketplace_cache_dir()?.join(slug);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)
                .with_context(|| format!("removing marketplace {}", dir.display()))?;
        }
        Ok(())
    }

    /// List the plugins declared by one cached marketplace plus their current
    /// installed status in the user's config dir.
    pub fn list_marketplace_plugins(slug: &str) -> Result<Vec<MarketplacePluginRecord>> {
        let repo_root = paths::marketplace_cache_dir()?.join(slug);
        let index = Self::load_marketplace_index(&repo_root)?;
        let installed: std::collections::HashSet<String> = Self::installed()
            .into_iter()
            .map(|plugin| plugin.name)
            .collect();
        let mut out: Vec<MarketplacePluginRecord> = index
            .plugins
            .into_iter()
            .map(|plugin| MarketplacePluginRecord {
                marketplace_slug: slug.to_string(),
                installed: installed.contains(&plugin.name),
                name: plugin.name,
                description: plugin.description,
                source: plugin.source,
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// Install a plugin from a marketplace: resolve its `source` path, copy the
    /// tree into `plugins_dir/<name>`, and record it as enabled.
    pub fn install(marketplace_slug: &str, plugin_name: &str) -> Result<()> {
        let repo_root = paths::marketplace_cache_dir()?.join(marketplace_slug);
        let index = Self::load_marketplace_index(&repo_root)?;
        let entry = index
            .plugins
            .iter()
            .find(|p| p.name == plugin_name)
            .with_context(|| {
                format!(
                    "plugin {} not in marketplace {}",
                    plugin_name, marketplace_slug
                )
            })?;

        let source = repo_root.join(entry.source.strip_prefix("./").unwrap_or(&entry.source));
        if !source.exists() {
            anyhow::bail!("plugin source {} missing", source.display());
        }
        let dest = paths::plugin_root(plugin_name)?;
        if dest.exists() {
            std::fs::remove_dir_all(&dest)
                .with_context(|| format!("removing old {}", dest.display()))?;
        }
        copy_tree(&source, &dest)
            .with_context(|| format!("copying {} -> {}", source.display(), dest.display()))?;
        Self::set_enabled(plugin_name, &dest, marketplace_slug)
    }

    /// Remove an installed plugin and drop it from the enabled list.
    pub fn uninstall(plugin_name: &str) -> Result<()> {
        let dest = paths::plugin_root(plugin_name)?;
        if dest.exists() {
            std::fs::remove_dir_all(&dest)
                .with_context(|| format!("removing {}", dest.display()))?;
        }
        Self::remove_enabled(plugin_name)
    }

    /// Installed plugins that loaders should scan, in stable (alphabetical) order.
    pub fn installed() -> Vec<InstalledPlugin> {
        let mut out = Vec::new();
        let Ok(file) = paths::enabled_plugins_file() else {
            return out;
        };
        let Ok(raw) = std::fs::read_to_string(&file) else {
            return out;
        };
        for line in raw.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            // Format: `<name>\t<marketplace_slug>` — the slug is metadata for
            // `plugin:` namespacing; the on-disk root is `plugins/<name>`.
            let (name, marketplace) = match line.split_once('\t') {
                Some((n, m)) => (n, m.to_string()),
                None => (line, String::new()),
            };
            let root = paths::plugin_root(name).unwrap_or_else(|_| PathBuf::from(name));
            if root.exists() {
                out.push(InstalledPlugin {
                    name: name.to_string(),
                    root,
                    marketplace,
                });
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Installed plugins plus the parsed manifest fields used by the plugin
    /// management UI.
    pub fn installed_details() -> Vec<InstalledPluginRecord> {
        let mut out = Vec::new();
        for plugin in Self::installed() {
            let manifest = load_plugin_manifest(&plugin.root);
            out.push(InstalledPluginRecord {
                name: plugin.name,
                marketplace: plugin.marketplace,
                root: plugin.root,
                description: manifest
                    .as_ref()
                    .ok()
                    .and_then(|manifest| manifest.description.clone()),
                version: manifest
                    .as_ref()
                    .ok()
                    .and_then(|manifest| manifest.version.clone()),
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    fn set_enabled(name: &str, _root: &Path, marketplace: &str) -> Result<()> {
        let file = paths::enabled_plugins_file()?;
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let existing = std::fs::read_to_string(&file).unwrap_or_default();
        let body = compose_enabled_body(&existing, name, marketplace);
        std::fs::write(&file, body).context("writing enabled_plugins")?;
        Ok(())
    }

    fn remove_enabled(name: &str) -> Result<()> {
        let file = paths::enabled_plugins_file()?;
        let existing = std::fs::read_to_string(&file).unwrap_or_default();
        let filtered: Vec<&str> = existing
            .lines()
            .filter(|l| {
                let key = l.split('\t').next().unwrap_or("");
                !l.trim().is_empty() && key != name
            })
            .collect();
        let body = filtered.join("\n");
        std::fs::write(
            &file,
            if body.is_empty() {
                body
            } else {
                format!("{body}\n")
            },
        )
        .context("writing enabled_plugins")?;
        Ok(())
    }
}

fn refresh_marketplace_dir(dir: &Path) -> Result<()> {
    if !dir.join(".git").exists() {
        anyhow::bail!("marketplace clone missing at {}", dir.display());
    }
    let fetch = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["fetch", "--all"])
        .status()
        .with_context(|| format!("git fetch in {}", dir.display()))?;
    if !fetch.success() {
        anyhow::bail!("git fetch failed for {}", dir.display());
    }
    let reset = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["reset", "--hard", "FETCH_HEAD"])
        .status()
        .with_context(|| format!("git reset in {}", dir.display()))?;
    if !reset.success() {
        anyhow::bail!("git reset failed for {}", dir.display());
    }
    Ok(())
}

fn git_remote_origin(dir: &Path) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["config", "--get", "remote.origin.url"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let value = String::from_utf8(out.stdout).ok()?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn load_plugin_manifest(root: &Path) -> Result<PluginManifest> {
    let path = root.join(".claude-plugin").join("plugin.json");
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("reading plugin manifest {}", path.display()))?;
    serde_json::from_str(&raw)
        .with_context(|| format!("parsing plugin manifest {}", path.display()))
}

/// Recursively copy a directory tree. `std::fs::copy` is per-file; a tree copy
/// needs a walk. Symlinks are copied as-is (resolved at read time by the
/// loaders), matching `cp -R` semantics on the platforms manox targets.
fn copy_tree(src: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dest.join(entry.file_name());
        let ft = entry.file_type()?;
        if ft.is_dir() {
            copy_tree(&from, &to)?;
        } else if ft.is_symlink() {
            let target = std::fs::read_link(&from)?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(&target, &to)?;
        } else {
            std::fs::copy(&from, &to).with_context(|| format!("copying {}", from.display()))?;
        }
    }
    Ok(())
}

/// Compose the enabled-plugins file body from `existing` plus a fresh entry.
/// Any prior line whose tab-split key equals `name` is dropped — by key, not
/// by prefix, so re-installing `foo` leaves `foobar` intact. Pure so it can be
/// tested without touching the real config path (HOME is process-global, so a
/// parallel test cannot safely redirect `paths::enabled_plugins_file`).
fn compose_enabled_body(existing: &str, name: &str, marketplace: &str) -> String {
    let filtered: Vec<&str> = existing
        .lines()
        .filter(|l| {
            let key = l.split('\t').next().unwrap_or("");
            !l.trim().is_empty() && key != name
        })
        .collect();
    let mut body = filtered.join("\n");
    if !body.is_empty() {
        body.push('\n');
    }
    body.push_str(name);
    body.push('\t');
    body.push_str(marketplace);
    body.push('\n');
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::marketplace_slug;

    #[test]
    fn marketplace_slug_strips_git_suffix() {
        assert_eq!(
            marketplace_slug("https://github.com/dspo/agent-marketplace.git"),
            "agent-marketplace"
        );
        assert_eq!(marketplace_slug("https://github.com/dspo/x"), "x");
    }

    #[test]
    fn parses_marketplace_index() {
        let raw = r#"{
            "name": "demo",
            "description": "d",
            "plugins": [
                {"name": "gitwork", "description": "g", "source": "./plugins/gitwork"}
            ]
        }"#;
        let idx: MarketplaceIndex = serde_json::from_str(raw).unwrap();
        assert_eq!(idx.name, "demo");
        assert_eq!(idx.plugins.len(), 1);
        assert_eq!(idx.plugins[0].source, "./plugins/gitwork");
    }

    #[test]
    fn parses_plugin_manifest() {
        let raw = r#"{"name":"mimo","description":"x","version":"0.2.0"}"#;
        let m: PluginManifest = serde_json::from_str(raw).unwrap();
        assert_eq!(m.name, "mimo");
        assert_eq!(m.version.as_deref(), Some("0.2.0"));
    }

    #[test]
    fn copy_tree_roundtrip() {
        let tmp = std::env::temp_dir().join("manox_plugin_copy_test");
        let _ = std::fs::remove_dir_all(&tmp);
        let src = tmp.join("src");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("a.txt"), "hi").unwrap();
        std::fs::write(src.join("sub").join("b.txt"), "yo").unwrap();
        let dest = tmp.join("dest");
        copy_tree(&src, &dest).unwrap();
        assert_eq!(std::fs::read_to_string(dest.join("a.txt")).unwrap(), "hi");
        assert_eq!(
            std::fs::read_to_string(dest.join("sub").join("b.txt")).unwrap(),
            "yo"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn compose_enabled_body_replaces_in_place_without_prefix_collision() {
        // Re-installing `foo` must drop only the prior `foo` line, leaving
        // `foobar` untouched. A naive `starts_with` filter would erase both.
        let existing = "foo\told-market\nfoobar\tmarket-a\n";
        let body = compose_enabled_body(existing, "foo", "new-market");
        let lines: Vec<&str> = body.lines().collect();
        assert!(lines.contains(&"foobar\tmarket-a"));
        assert!(lines.contains(&"foo\tnew-market"));
        assert!(!lines.contains(&"foo\told-market"));
    }

    #[test]
    fn compose_enabled_body_inserts_when_absent() {
        let body = compose_enabled_body("", "gitwork", "agent-marketplace");
        assert_eq!(body, "gitwork\tagent-marketplace\n");
    }
}
