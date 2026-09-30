//! Plugin manager for the Claude Code marketplace ecosystem, sharing the
//! Claude Code home in place (`~/.claude/plugins/`).
//!
//! manox consumes the same on-disk state Claude Code maintains, so an
//! ecosystem asset installed by either tool is visible to both:
//!
//! - `installed_plugins.json` (schema `version: 2`) — the install registry.
//!   Each key is `<plugin>@<marketplace>` mapping to install entries whose
//!   `installPath` points at a versioned tree under
//!   `cache/<marketplace>/<plugin>/<version>/`. Claude Code may rewrite this
//!   file at any time (installs, updates, cache GC), so it is re-read on
//!   every call and never cached across calls.
//! - `known_marketplaces.json` — marketplace registrations keyed by slug,
//!   each carrying its git source and clone location.
//! - `settings.json` — the `enabledPlugins` map (`<plugin>@<marketplace>` →
//!   bool) is the enable/disable surface. manox edits only that key and
//!   round-trips the rest of the document untouched (the file also carries
//!   Claude Code's own hooks/env/model settings and has no cross-process
//!   lock, so writes re-read immediately before an atomic temp+rename
//!   replace).
//!
//! Both registry files are Claude Code's private implementation details (no
//! stability contract), so the contract here is defensive: unknown fields
//! round-trip through a `Value` view, malformed entries are skipped with a
//! warning, a schema `version` above the known one degrades manox to
//! read-only, an unparseable file is never overwritten, and a failed read
//! yields an empty set rather than blocking startup.
//!
//! Marketplace clones live under `plugins/marketplaces/<slug>/`; installed
//! trees under `plugins/cache/<marketplace>/<plugin>/<version>/`. A
//! best-effort `.in_use/<pid>` marker is written beside each used install —
//! Claude Code's cache GC prunes dead pids, so the marker keeps a tree manox
//! is running from alive and self-heals when the process exits.
//!
//! Writes happen only on explicit user operations (install / uninstall /
//! enable / disable / add / remove / refresh marketplace). `git` is shelled
//! out to (never a `git2` dependency — the project forbids vendored deps and
//! `git2` would drag libgit2 into the binary).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result, bail};
use chrono::SecondsFormat;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::paths;

// ---------------------------------------------------------------------------
// Marketplace index (`<clone>/.claude-plugin/marketplace.json`)
// ---------------------------------------------------------------------------

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
    pub source: MarketplacePluginSource,
}

impl MarketplacePluginSource {
    /// Human-readable form for UI rows (`./plugins/gitwork`,
    /// `github:owner/repo`, `git-subdir:<url>@<ref>`).
    pub fn display(&self) -> String {
        match self {
            MarketplacePluginSource::Relative(path) => path.clone(),
            MarketplacePluginSource::Remote(remote) => match remote {
                RemoteSource::GitHub { repo, .. } => format!("github:{repo}"),
                RemoteSource::GitSubdir { url, r#ref, .. } => match r#ref {
                    Some(r#ref) => format!("git-subdir:{url}@{ref}"),
                    None => format!("git-subdir:{url}"),
                },
            },
            MarketplacePluginSource::Unsupported { kind } => format!("unsupported:{kind}"),
        }
    }

    /// Whether `install` can act on this source at all — `Unsupported`
    /// entries stay visible in marketplace listings but refuse to install.
    pub fn installable(&self) -> bool {
        !matches!(self, MarketplacePluginSource::Unsupported { .. })
    }
}

/// The `source` field of a marketplace entry. The official
/// `claude-plugins-official` index in the wild (2026-09) carries four object
/// kinds — `github` (2/203), `git-subdir` (47), `url` (104), all of them
/// usually sha-pinned — plus 50 plain in-repo relative-path strings. `url`
/// is a whole-repo clone pinned by sha, i.e. a path-less `git-subdir`, and
/// deserializes into the same variant.
///
/// An unknown kind must not fail the enclosing entry, let alone the whole
/// index: one unrecognized source used to make the entire marketplace
/// vanish from the UI. Unknown kinds parse into
/// [`MarketplacePluginSource::Unsupported`] — listed, never installed.
#[derive(Debug, Clone)]
pub enum MarketplacePluginSource {
    /// Directory relative to the marketplace repo root (`./plugins/gitwork`).
    Relative(String),
    Remote(RemoteSource),
    /// A source kind this build does not understand, kept visible instead of
    /// poisoning the index parse.
    Unsupported {
        kind: String,
    },
}

#[derive(Debug, Clone)]
pub enum RemoteSource {
    /// `{source: "github", repo: "owner/repo", ref?, sha?}` — resolved as a
    /// shallow clone of `https://github.com/<repo>.git`.
    GitHub {
        repo: String,
        r#ref: Option<String>,
        sha: Option<String>,
    },
    /// `{source: "git-subdir", url, path?, ref?, sha?}` — a subdirectory of
    /// an arbitrary git repo. `{source: "url", url, sha?}` deserializes here
    /// with `path: None`: a whole-repo clone pinned by sha.
    GitSubdir {
        url: String,
        path: Option<String>,
        r#ref: Option<String>,
        sha: Option<String>,
    },
}

/// Hand-rolled (not `#[serde(untagged)]`) so a mis-shaped `source` reports
/// which shape was expected; unknown kinds degrade to
/// [`MarketplacePluginSource::Unsupported`] rather than erroring.
impl<'de> Deserialize<'de> for MarketplacePluginSource {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = Value::deserialize(deserializer)?;
        let unsupported = |kind: &str| {
            Ok(MarketplacePluginSource::Unsupported {
                kind: kind.to_string(),
            })
        };
        match raw {
            Value::String(path) => Ok(MarketplacePluginSource::Relative(path)),
            Value::Object(map) => {
                let kind = map.get("source").and_then(Value::as_str);
                match kind {
                    Some("github") => {
                        let Some(repo) = map.get("repo").and_then(Value::as_str) else {
                            return unsupported("github-without-repo");
                        };
                        Ok(MarketplacePluginSource::Remote(RemoteSource::GitHub {
                            repo: repo.to_string(),
                            r#ref: map.get("ref").and_then(Value::as_str).map(str::to_string),
                            sha: map.get("sha").and_then(Value::as_str).map(str::to_string),
                        }))
                    }
                    Some("git-subdir") => {
                        let Some(url) = map.get("url").and_then(Value::as_str) else {
                            return unsupported("git-subdir-without-url");
                        };
                        Ok(MarketplacePluginSource::Remote(RemoteSource::GitSubdir {
                            url: url.to_string(),
                            path: map.get("path").and_then(Value::as_str).map(str::to_string),
                            r#ref: map.get("ref").and_then(Value::as_str).map(str::to_string),
                            sha: map.get("sha").and_then(Value::as_str).map(str::to_string),
                        }))
                    }
                    Some("url") => {
                        let Some(url) = map.get("url").and_then(Value::as_str) else {
                            return unsupported("url-without-url");
                        };
                        Ok(MarketplacePluginSource::Remote(RemoteSource::GitSubdir {
                            url: url.to_string(),
                            path: None,
                            r#ref: map.get("ref").and_then(Value::as_str).map(str::to_string),
                            sha: map.get("sha").and_then(Value::as_str).map(str::to_string),
                        }))
                    }
                    Some(kind) => unsupported(kind),
                    None => unsupported("malformed"),
                }
            }
            _ => unsupported("malformed"),
        }
    }
}

// ---------------------------------------------------------------------------
// Public records (shapes consumed by the downstream plugin-management UI)
// ---------------------------------------------------------------------------

/// A plugin root that loaders should scan: the installed plugin's directory
/// plus the marketplace slug it was installed from (for namespacing and
/// `plugin:` qualified lookups).
#[derive(Debug, Clone)]
pub struct InstalledPlugin {
    pub name: String,
    pub root: PathBuf,
    pub marketplace: String,
    /// The full registry key (`name@marketplace`) this install was read
    /// from — the identity `enabledPlugins` toggles are keyed by.
    pub key: String,
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
    /// Whether the plugin is installed and not explicitly disabled. Meaningful
    /// only when `installed` is true.
    pub enabled: bool,
}

#[derive(Debug, Clone)]
pub struct InstalledPluginRecord {
    pub name: String,
    pub marketplace: String,
    pub root: PathBuf,
    pub description: Option<String>,
    pub version: Option<String>,
    /// Whether the plugin is currently enabled (not explicitly disabled in
    /// `settings.json`). A missing `enabledPlugins` key counts as enabled:
    /// the install registry is the fact, the map is a UI toggle.
    pub enabled: bool,
}

// ---------------------------------------------------------------------------
// Filesystem-backed plugin manager
// ---------------------------------------------------------------------------

pub struct PluginManager;

impl PluginManager {
    // -- marketplaces -------------------------------------------------------

    /// Register (or refresh) a marketplace from its git URL and return its
    /// index. If `known_marketplaces.json` already registers the same URL
    /// under a different slug (Claude Code keys by its own name, e.g. for
    /// `github:`-sourced marketplaces), that slug and clone are reused so
    /// the two tools never fork one marketplace into two registrations.
    pub fn add_marketplace(git_url: &str) -> Result<MarketplaceIndex> {
        let known = KnownMarketplaces::load()?;
        let (slug, root) = match known.by_url(git_url) {
            Some(record) => (record.slug.clone(), record.install_location.clone()),
            None => {
                let slug = paths::marketplace_slug(git_url);
                let root = marketplace_root_fallback(&slug);
                (slug, root)
            }
        };
        clone_or_refresh(&root, git_url)?;
        let index = Self::load_marketplace_index(&root)?;
        KnownMarketplaces::register(&slug, git_url, &root)?;
        Ok(index)
    }

    /// Remove a cloned marketplace repo by git URL. Installed plugins copied
    /// into `cache/` stay installed — they are self-contained trees.
    pub fn remove_marketplace(git_url: &str) -> Result<()> {
        let known = KnownMarketplaces::load()?;
        let Some(record) = known.by_url(git_url) else {
            return Self::remove_marketplace_by_slug(&paths::marketplace_slug(git_url));
        };
        Self::remove_marketplace_by_slug(&record.slug)
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

    /// List marketplace slugs present in the store (registered ones plus any
    /// clone directories on disk).
    pub fn list_marketplaces() -> Vec<String> {
        let mut out: Vec<String> = KnownMarketplaces::load()
            .map(|known| known.0.keys().cloned().collect())
            .unwrap_or_default();
        if let Some(dir) = marketplaces_root()
            && let Ok(entries) = std::fs::read_dir(&dir)
        {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.join(".git").exists()
                    && let Ok(name) = entry.file_name().into_string()
                    && !out.contains(&name)
                {
                    out.push(name);
                }
            }
        }
        out.sort();
        out
    }

    /// Rich view of cached marketplaces for UI surfaces. Reads the current
    /// on-disk index and known git source for each clone.
    pub fn list_marketplace_records() -> Vec<MarketplaceRecord> {
        let known = KnownMarketplaces::load().unwrap_or_default();
        let mut out = Vec::new();
        for slug in Self::list_marketplaces() {
            let root = known
                .root_of(&slug)
                .unwrap_or_else(|| marketplace_root_fallback(&slug));
            if !root.join(".git").exists() {
                continue;
            }
            let Ok(index) = Self::load_marketplace_index(&root) else {
                continue;
            };
            let git_url = known
                .0
                .get(&slug)
                .and_then(KnownMarketplace::git_url)
                .or_else(|| git_remote_origin(&root));
            out.push(MarketplaceRecord {
                slug,
                git_url,
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
        let known = KnownMarketplaces::load()?;
        let root = known
            .root_of(slug)
            .unwrap_or_else(|| marketplace_root_fallback(slug));
        refresh_marketplace_dir(&root)?;
        KnownMarketplaces::touch(slug)?;
        Self::load_marketplace_index(&root)
    }

    /// Remove a cached marketplace clone by slug (clone and registration go;
    /// installed plugins stay).
    pub fn remove_marketplace_by_slug(slug: &str) -> Result<()> {
        let known = KnownMarketplaces::load()?;
        let root = known
            .root_of(slug)
            .unwrap_or_else(|| marketplace_root_fallback(slug));
        if root.exists() {
            std::fs::remove_dir_all(&root)
                .with_context(|| format!("removing marketplace {}", root.display()))?;
        }
        KnownMarketplaces::unregister(slug)
    }

    /// List the plugins declared by one cached marketplace plus their current
    /// installed/enabled status in the shared store. Enabled-ness is keyed
    /// `name@<this marketplace>` — the same name installed from another
    /// marketplace has its own toggle and does not leak here.
    pub fn list_marketplace_plugins(slug: &str) -> Result<Vec<MarketplacePluginRecord>> {
        let known = KnownMarketplaces::load()?;
        let repo_root = known
            .root_of(slug)
            .unwrap_or_else(|| marketplace_root_fallback(slug));
        let index = Self::load_marketplace_index(&repo_root)?;
        let installed_names: std::collections::HashSet<String> =
            Self::all_installed().into_iter().map(|p| p.name).collect();
        let toggles = enabled_toggles();
        let mut out: Vec<MarketplacePluginRecord> = index
            .plugins
            .into_iter()
            .map(|plugin| {
                let installed = installed_names.contains(&plugin.name);
                let key = format!("{}@{}", plugin.name, slug);
                let enabled = installed && toggles.get(&key).copied() != Some(false);
                MarketplacePluginRecord {
                    marketplace_slug: slug.to_string(),
                    installed,
                    enabled,
                    name: plugin.name,
                    description: plugin.description,
                    source: plugin.source.display(),
                }
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    // -- plugins ------------------------------------------------------------

    /// Install a plugin from a marketplace: materialize its source tree (an
    /// in-repo path, a subdirectory of a remote repo, or a GitHub repo), copy
    /// it into the versioned cache location, and record the install in
    /// `installed_plugins.json` + `enabledPlugins`. Reinstalling preserves
    /// the original `installedAt` stamp and replaces the tree.
    pub fn install(marketplace_slug: &str, plugin_name: &str) -> Result<()> {
        let known = KnownMarketplaces::load()?;
        let repo_root = known
            .root_of(marketplace_slug)
            .unwrap_or_else(|| marketplace_root_fallback(marketplace_slug));
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

        // The caller owns the scratch dir: a remote-source clone must outlive
        // materialize_source or the copy below would read a deleted directory.
        let scratch = tempfile::tempdir().context("creating install scratch dir")?;
        let (source_tree, commit_sha) =
            materialize_source(&repo_root, &entry.source, scratch.path())?;
        let version = load_plugin_manifest(&source_tree)
            .ok()
            .and_then(|m| m.version)
            .unwrap_or_else(|| "0.0.0".to_string());
        let dest = paths::plugins_dir()?
            .join("cache")
            .join(marketplace_slug)
            .join(plugin_name)
            .join(&version);
        if dest.exists() {
            std::fs::remove_dir_all(&dest)
                .with_context(|| format!("removing old {}", dest.display()))?;
        }
        copy_tree(&source_tree, &dest)
            .with_context(|| format!("copying {} -> {}", source_tree.display(), dest.display()))?;

        InstalledPlugins::record_install(
            plugin_name,
            marketplace_slug,
            &dest,
            &version,
            &commit_sha,
        )?;
        if let Err(err) =
            SettingsPatch::set_enabled(&format!("{plugin_name}@{marketplace_slug}"), true)
        {
            // Non-fatal: a missing `enabledPlugins` key already reads as
            // enabled, so the install is usable even if the toggle write lost.
            tracing::warn!("installed {plugin_name} but could not update settings: {err:#}");
        }
        Ok(())
    }

    /// Remove an installed plugin: drop its cache tree and both registry
    /// entries. A disabled plugin is removed the same way as an enabled one.
    pub fn uninstall(plugin_name: &str) -> Result<()> {
        for key in Self::keys_for(plugin_name)? {
            let mut doc = InstalledPlugins::load()?;
            // Only the user-scope trees are manox's to delete: sibling
            // project/local entries are Claude Code's per-repo installs, and
            // removing them would destroy exactly what `record_install`
            // preserves on the write side.
            for entry in doc.entries(&key).into_iter().filter(|e| e.scope == "user") {
                if entry.root.exists() {
                    std::fs::remove_dir_all(&entry.root).with_context(|| {
                        format!("removing installed tree {}", entry.root.display())
                    })?;
                }
            }
            // The toggle goes with the key: a surviving sibling scope still
            // needs its `enabledPlugins` entry intact for Claude Code.
            if doc.remove_user_scope(&key)? {
                SettingsPatch::remove_enabled(&key)?;
            }
        }
        Ok(())
    }

    /// Re-enable an installed plugin: set `enabledPlugins[key] = true` so
    /// loaders scan it again on the next start.
    pub fn enable(plugin_name: &str) -> Result<()> {
        for key in Self::keys_for(plugin_name)? {
            SettingsPatch::set_enabled(&key, true)?;
        }
        Ok(())
    }

    /// Disable an installed plugin: set `enabledPlugins[key] = false` so
    /// loaders stop scanning it on the next start. Files stay on disk — only
    /// `uninstall` removes them.
    pub fn disable(plugin_name: &str) -> Result<()> {
        for key in Self::keys_for(plugin_name)? {
            SettingsPatch::set_enabled(&key, false)?;
        }
        Ok(())
    }

    /// Plugins that loaders should scan — the enabled subset of installed
    /// plugins, in stable (alphabetical) order. Re-read from disk on every
    /// call: Claude Code may rewrite the registry at any time.
    pub fn installed() -> Vec<InstalledPlugin> {
        Self::scan()
            .into_iter()
            .filter(|(_, enabled)| *enabled)
            .map(|(plugin, _)| plugin)
            .collect()
    }

    /// Installed plugins regardless of enabled state, in stable (alphabetical)
    /// order. Entries whose install tree vanished (Claude Code cache GC, a
    /// failed update) are skipped silently — the next read reflects whatever
    /// Claude Code settled on. Keys with no user-scope entry (project/local
    /// scope installs Claude Code records per-repo) are *not* scanned: the
    /// user-level loaders would otherwise promote a repo-scoped install into
    /// every session.
    pub fn all_installed() -> Vec<InstalledPlugin> {
        Self::scan().into_iter().map(|(plugin, _)| plugin).collect()
    }

    /// The single read pass behind [`Self::installed`], [`Self::all_installed`]
    /// and [`Self::installed_details`]: one read each of
    /// `installed_plugins.json` and `settings.json`, yielding every
    /// user-scope install together with its key-level toggle. Toggles are
    /// keyed `name@marketplace` — a name present under two marketplaces is
    /// two independent toggles, and filtering by bare name would let an
    /// explicit `foo@m1: false` keep scanning because `foo@m2` is enabled.
    /// A missing key counts as enabled: the install registry is the fact,
    /// the map is a UI toggle.
    fn scan() -> Vec<(InstalledPlugin, bool)> {
        let Ok(plugins) = InstalledPlugins::load() else {
            return Vec::new();
        };
        let toggles = enabled_toggles();
        let mut out = Vec::new();
        for key in plugins.keys() {
            let Some((name, marketplace)) = key.rsplit_once('@') else {
                tracing::warn!("skipping malformed installed-plugins key {key:?}");
                continue;
            };
            // Only the user-scope entry is the one the user-level loaders
            // scan; a key without one belongs to other scopes' bookkeeping.
            let Some(entry) = plugins.entries(key).into_iter().find(|e| e.scope == "user") else {
                continue;
            };
            if !entry.root.exists() {
                continue;
            }
            mark_in_use(&entry.root);
            let enabled = toggles.get(key).copied() != Some(false);
            out.push((
                InstalledPlugin {
                    name: name.to_string(),
                    root: entry.root,
                    marketplace: marketplace.to_string(),
                    key: key.to_string(),
                },
                enabled,
            ));
        }
        out.sort_by(|a, b| {
            (a.0.name.clone(), a.0.key.clone()).cmp(&(b.0.name.clone(), b.0.key.clone()))
        });
        out
    }

    /// Installed plugins plus the parsed manifest fields used by the plugin
    /// management UI.
    pub fn installed_details() -> Vec<InstalledPluginRecord> {
        let mut out = Vec::new();
        for (plugin, enabled) in Self::scan() {
            let manifest = load_plugin_manifest(&plugin.root);
            out.push(InstalledPluginRecord {
                name: plugin.name.clone(),
                marketplace: plugin.marketplace.clone(),
                root: plugin.root.clone(),
                description: manifest
                    .as_ref()
                    .ok()
                    .and_then(|manifest| manifest.description.clone()),
                version: manifest
                    .as_ref()
                    .ok()
                    .and_then(|manifest| manifest.version.clone()),
                enabled,
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// The registry keys whose name part is `plugin_name`; empty when the
    /// plugin has no install registered, keeping the toggles free of names
    /// with nothing behind them.
    fn keys_for(plugin_name: &str) -> Result<Vec<String>> {
        let plugins = InstalledPlugins::load()?;
        let keys: Vec<String> = plugins
            .keys()
            .iter()
            .filter(|key| key.rsplit_once('@').is_some_and(|(n, _)| n == plugin_name))
            .cloned()
            .collect();
        if keys.is_empty() {
            bail!("plugin {plugin_name} is not installed");
        }
        Ok(keys)
    }
}

/// The `enabledPlugins` map of `settings.json`, read leniently (a missing or
/// malformed file yields no toggles — every install then reads as enabled).
fn enabled_toggles() -> BTreeMap<String, bool> {
    paths::claude_settings_file()
        .ok()
        .and_then(|path| read_json(&path).ok())
        .flatten()
        .and_then(|doc| doc.get("enabledPlugins").cloned())
        .and_then(|t| t.as_object().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(k, v)| v.as_bool().map(|b| (k, b)))
        .collect()
}

/// Default clone location for a slug when no explicit `installLocation` is
/// registered: `plugins/marketplaces/<slug>`.
fn marketplace_root_fallback(slug: &str) -> PathBuf {
    paths::plugins_dir()
        .map(|p| p.join("marketplaces").join(slug))
        .unwrap_or_default()
}

fn marketplaces_root() -> Option<PathBuf> {
    paths::plugins_dir().map(|p| p.join("marketplaces")).ok()
}

// ---------------------------------------------------------------------------
// installed_plugins.json (schema version 2)
// ---------------------------------------------------------------------------

/// One install entry of `installed_plugins.json`. The document is read and
/// written through a `Value` round-trip, so fields manox does not model
/// survive a write untouched.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
struct InstallEntry {
    scope: String,
    #[serde(rename = "installPath")]
    root: PathBuf,
    #[serde(default)]
    version: String,
    #[serde(default, rename = "installedAt")]
    installed_at: String,
    #[serde(default, rename = "lastUpdated")]
    last_updated: String,
    /// Optional in the wild: current Claude Code omits it when the
    /// marketplace entry carries no sha (observed on huggingface-skills).
    /// Requiring it made the installed plugin silently invisible — the
    /// entry failed to parse and was skipped as malformed.
    #[serde(default, rename = "gitCommitSha")]
    commit_sha: String,
}

/// The highest `installed_plugins.json` schema version manox knows how to
/// write; a newer document degrades manox to read-only instead of corrupting
/// Claude Code's registry.
const SUPPORTED_VERSION: u64 = 2;

struct InstalledPlugins {
    doc: Value,
    version: u64,
    keys: Vec<String>,
}

impl InstalledPlugins {
    /// Parse the document for reads. A missing file yields an empty document;
    /// an unparseable one yields an empty document plus a warning — readers
    /// must never block startup on Claude Code's file.
    fn load() -> Result<Self> {
        let path = paths::installed_plugins_file()?;
        let doc = read_json(&path)?.unwrap_or_else(|| json!({}));
        Ok(Self::from_doc(path, doc))
    }

    /// Parse the document for a write: unlike [`Self::load`], an unparseable
    /// file is an error — the write must not replace a document it could not
    /// read with a fresh, empty one.
    fn load_for_write() -> Result<Self> {
        let path = paths::installed_plugins_file()?;
        let doc = read_json_for_write(&path)?.unwrap_or_else(|| json!({}));
        let parsed = Self::from_doc(path, doc);
        parsed.writable()?;
        Ok(parsed)
    }

    fn from_doc(path: PathBuf, doc: Value) -> Self {
        if !doc.is_object() {
            tracing::warn!(
                "installed_plugins.json at {} is not an object; treating as empty",
                path.display()
            );
            return Self {
                doc: json!({}),
                version: 0,
                keys: Vec::new(),
            };
        }
        let version = doc.get("version").and_then(Value::as_u64).unwrap_or(2);
        let mut keys: Vec<String> = doc
            .get("plugins")
            .and_then(Value::as_object)
            .map(|plugins| plugins.keys().cloned().collect())
            .unwrap_or_default();
        keys.sort();
        Self { doc, version, keys }
    }

    fn writable(&self) -> Result<()> {
        if self.version > SUPPORTED_VERSION {
            bail!(
                "installed_plugins.json schema version {} is newer than supported \
                 ({SUPPORTED_VERSION}); refusing to write — let Claude Code manage installs",
                self.version
            );
        }
        Ok(())
    }

    /// The registry keys, in sorted order.
    fn keys(&self) -> &[String] {
        &self.keys
    }

    /// Raw view of one key's install entries, unknown fields included — the
    /// write path needs these to round-trip entries manox does not model.
    fn raw_entries(&self, key: &str) -> Vec<Value> {
        self.doc
            .get("plugins")
            .and_then(|p| p.get(key))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    }

    /// Typed view of one key's install entries; malformed entries are
    /// dropped with a warning rather than failing the whole registry.
    fn entries(&self, key: &str) -> Vec<InstallEntry> {
        self.raw_entries(key)
            .iter()
            .filter_map(
                |raw| match serde_json::from_value::<InstallEntry>(raw.clone()) {
                    Ok(entry) => Some(entry),
                    Err(err) => {
                        tracing::warn!("skipping malformed install entry for {key}: {err}");
                        None
                    }
                },
            )
            .collect()
    }

    fn record_install(
        name: &str,
        marketplace: &str,
        root: &Path,
        version: &str,
        commit_sha: &str,
    ) -> Result<()> {
        let mut doc = Self::load_for_write()?;
        let key = format!("{name}@{marketplace}");
        let now = now_rfc3339();
        // A key may carry sibling entries Claude Code wrote for other scopes
        // (project/local installs of the same plugin); replacing the array
        // wholesale would silently delete those registrations. Drop only the
        // user-scope entries being superseded and keep the rest verbatim.
        let raws = doc.raw_entries(&key);
        let siblings: Vec<Value> = raws
            .iter()
            .filter(|raw| raw.get("scope").and_then(Value::as_str) != Some("user"))
            .cloned()
            .collect();
        let previous_installed_at = raws
            .iter()
            .filter_map(|raw| serde_json::from_value::<InstallEntry>(raw.clone()).ok())
            .find(|entry| entry.scope == "user")
            .map(|entry| entry.installed_at)
            .unwrap_or_else(|| now.clone());
        let entry = json!({
            "scope": "user",
            "installPath": root,
            "version": version,
            "installedAt": previous_installed_at,
            "lastUpdated": now,
            "gitCommitSha": commit_sha,
        });
        let obj = doc.doc.as_object_mut().context("installed_plugins doc")?;
        obj.insert("version".to_string(), json!(SUPPORTED_VERSION));
        let plugins = obj
            .entry("plugins")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .context("installed_plugins.plugins")?;
        let mut list = siblings;
        list.push(entry);
        plugins.insert(key, Value::Array(list));
        write_json_atomic(&paths::installed_plugins_file()?, &doc.doc)
    }

    /// Drop the user-scope entries under `key`, keeping sibling scopes
    /// verbatim; returns true when the key is now entry-less and was removed
    /// from the registry altogether. A key with surviving siblings stays
    /// registered — it is still Claude Code's bookkeeping for those scopes.
    fn remove_user_scope(&mut self, key: &str) -> Result<bool> {
        let mut doc = Self::load_for_write()?;
        let Some(list) = doc
            .doc
            .get_mut("plugins")
            .and_then(|p| p.get_mut(key))
            .and_then(Value::as_array_mut)
            .map(|list| {
                list.retain(|raw| raw.get("scope").and_then(Value::as_str) != Some("user"));
                list.clone()
            })
        else {
            return Ok(false);
        };
        if list.is_empty() {
            let removed = doc
                .doc
                .get_mut("plugins")
                .and_then(Value::as_object_mut)
                .is_some_and(|plugins| plugins.remove(key).is_some());
            if removed {
                write_json_atomic(&paths::installed_plugins_file()?, &doc.doc)?;
                return Ok(true);
            }
            return Ok(false);
        }
        write_json_atomic(&paths::installed_plugins_file()?, &doc.doc)?;
        Ok(false)
    }
}

// ---------------------------------------------------------------------------
// known_marketplaces.json
// ---------------------------------------------------------------------------

/// One `known_marketplaces.json` record (parsed leniently; the raw `Value` is
/// what gets round-tripped so Claude Code's exact shapes survive).
#[derive(Debug, Clone)]
struct KnownMarketplace {
    slug: String,
    install_location: PathBuf,
    raw: Value,
}

impl KnownMarketplace {
    /// The git URL this marketplace was registered from, if known.
    fn git_url(&self) -> Option<String> {
        let source = self.raw.get("source")?;
        match source.get("source").and_then(Value::as_str)? {
            "git" => source
                .get("url")
                .and_then(Value::as_str)
                .map(str::to_string),
            "github" => source
                .get("repo")
                .and_then(Value::as_str)
                .map(|repo| format!("https://github.com/{repo}.git")),
            _ => None,
        }
    }
}

/// The `known_marketplaces.json` document keyed by slug.
#[derive(Default)]
struct KnownMarketplaces(BTreeMap<String, KnownMarketplace>);

impl KnownMarketplaces {
    fn load() -> Result<Self> {
        let path = paths::known_marketplaces_file()?;
        let doc = read_json(&path)?.unwrap_or_else(|| json!({}));
        let mut map = BTreeMap::new();
        if let Some(obj) = doc.as_object() {
            for (slug, raw) in obj {
                let install_location = raw
                    .get("installLocation")
                    .and_then(Value::as_str)
                    .map(PathBuf::from)
                    .unwrap_or_else(|| marketplace_root_fallback(slug));
                map.insert(
                    slug.clone(),
                    KnownMarketplace {
                        slug: slug.clone(),
                        install_location,
                        raw: raw.clone(),
                    },
                );
            }
        }
        Ok(Self(map))
    }

    fn root_of(&self, slug: &str) -> Option<PathBuf> {
        self.0.get(slug).map(|r| r.install_location.clone())
    }

    /// The registration whose source URL matches `git_url` (exact `git` URL
    /// or a `github` repo expansion), so a marketplace Claude Code already
    /// knows is never registered twice under two slugs.
    fn by_url(&self, git_url: &str) -> Option<&KnownMarketplace> {
        let want = normalize_git_url(git_url);
        self.0.values().find(|record| {
            record
                .git_url()
                .is_some_and(|url| normalize_git_url(&url) == want)
        })
    }

    /// Insert or update one registration, merging into any existing record
    /// instead of replacing it: only `installLocation`/`lastUpdated` are
    /// manox's to maintain, and `source` is written only when the slug has
    /// no source yet — rewriting a Claude Code-authored registration (say a
    /// `{"source": "github", "repo": ...}` shape, or records carrying extra
    /// metadata fields) would fork exactly the shared registry this module
    /// exists to share. An unparseable file aborts the write.
    fn register(slug: &str, git_url: &str, root: &Path) -> Result<()> {
        let path = paths::known_marketplaces_file()?;
        let mut doc = read_json_for_write(&path)?.unwrap_or_else(|| json!({}));
        let obj = doc.as_object_mut().context("known_marketplaces doc")?;
        let entry = obj
            .entry(slug.to_string())
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .context("known_marketplaces entry")?;
        if !entry.contains_key("source") {
            entry.insert(
                "source".to_string(),
                json!({"source": "git", "url": git_url}),
            );
        } else if let Some(existing_url) = entry
            .get("source")
            .and_then(|s| s.get("url"))
            .and_then(Value::as_str)
            .filter(|existing| normalize_git_url(existing) != normalize_git_url(git_url))
        {
            // A source naming a *different* URL under the slug manox just
            // cloned is a stale or colliding registration — surfacing it
            // beats silently trusting either side.
            tracing::warn!(
                "marketplace {slug} was registered for {existing_url} but {} was cloned into it",
                git_url
            );
        }
        entry.insert("installLocation".to_string(), json!(root));
        entry.insert("lastUpdated".to_string(), json!(now_rfc3339()));
        write_json_atomic(&path, &doc)
    }

    /// Bump `lastUpdated` for one slug (a refresh happened).
    fn touch(slug: &str) -> Result<()> {
        let path = paths::known_marketplaces_file()?;
        let mut doc = read_json_for_write(&path)?.unwrap_or_else(|| json!({}));
        let touched = doc
            .get_mut(slug)
            .and_then(Value::as_object_mut)
            .is_some_and(|entry| {
                entry.insert("lastUpdated".to_string(), json!(now_rfc3339()));
                true
            });
        if touched {
            write_json_atomic(&path, &doc)?;
        }
        Ok(())
    }

    fn unregister(slug: &str) -> Result<()> {
        let path = paths::known_marketplaces_file()?;
        let mut doc = read_json_for_write(&path)?.unwrap_or_else(|| json!({}));
        let removed = doc
            .as_object_mut()
            .is_some_and(|obj| obj.remove(slug).is_some());
        if removed {
            write_json_atomic(&path, &doc)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// settings.json (`enabledPlugins` key only)
// ---------------------------------------------------------------------------

/// Read-modify-write of the `enabledPlugins` map inside `settings.json`.
/// The file carries Claude Code's own settings with no cross-process lock:
/// re-read immediately before each write and replace atomically, so a lost
/// update can only drop a concurrent plugin toggle, never unrelated keys.
/// An unparseable file aborts the write instead of being replaced.
struct SettingsPatch;

impl SettingsPatch {
    fn set_enabled(key: &str, enabled: bool) -> Result<()> {
        let path = paths::claude_settings_file()?;
        let mut doc = read_json_for_write(&path)?.unwrap_or_else(|| json!({}));
        let obj = doc.as_object_mut().context("settings.json doc")?;
        let toggles = obj
            .entry("enabledPlugins")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .context("enabledPlugins")?;
        toggles.insert(key.to_string(), json!(enabled));
        write_json_atomic(&path, &doc)
    }

    fn remove_enabled(key: &str) -> Result<()> {
        let path = paths::claude_settings_file()?;
        let mut doc = read_json_for_write(&path)?.unwrap_or_else(|| json!({}));
        let removed = doc
            .get_mut("enabledPlugins")
            .and_then(Value::as_object_mut)
            .is_some_and(|toggles| toggles.remove(key).is_some());
        if removed {
            write_json_atomic(&path, &doc)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Shared JSON helpers
// ---------------------------------------------------------------------------

/// Read a JSON document for reads: `Ok(None)` when absent, warn + `None`
/// when unparseable — readers must never block startup on Claude Code's
/// files.
fn read_json(path: &Path) -> Result<Option<Value>> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            tracing::warn!("failed to read {}: {e}", path.display());
            return Ok(None);
        }
    };
    match serde_json::from_str(&raw) {
        Ok(doc) => Ok(Some(doc)),
        Err(err) => {
            tracing::warn!("failed to parse {}: {err}", path.display());
            Ok(None)
        }
    }
}

/// Read a JSON document for a write: unlike [`read_json`], an unparseable
/// file is an error — a write must never replace a document it could not
/// read with a fresh, empty one.
fn read_json_for_write(path: &Path) -> Result<Option<Value>> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    serde_json::from_str(&raw)
        .map(Some)
        .with_context(|| format!("parsing {}", path.display()))
}

/// Atomic replace: write a sibling temp file, then rename over the target.
/// Claude Code reads these files without locking, so a torn write would be
/// visible to a concurrent reader; a rename never is.
fn write_json_atomic(path: &Path, doc: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut body = serde_json::to_string_pretty(doc).context("serializing JSON document")?;
    body.push('\n');
    let tmp = path.with_extension("json.manox-tmp");
    std::fs::write(&tmp, body).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Exact-URL comparison modulo a trailing `.git` / `/`, so
/// `https://github.com/dspo/x.git` matches `https://github.com/dspo/x`.
fn normalize_git_url(url: &str) -> String {
    url.trim()
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .to_string()
}

// ---------------------------------------------------------------------------
// git plumbing
// ---------------------------------------------------------------------------

/// Clone (or fast-forward update) a marketplace repo into `root`. A second
/// add for the same URL refreshes the existing clone rather than failing —
/// mirroring Claude Code, which re-pulls on re-registration.
fn clone_or_refresh(root: &Path, git_url: &str) -> Result<()> {
    if root.join(".git").exists() {
        refresh_marketplace_dir(root)
    } else {
        if let Some(parent) = root.parent() {
            std::fs::create_dir_all(parent).context("creating marketplaces dir")?;
        }
        let status = Command::new("git")
            .args(["clone", "--depth", "1", git_url])
            .arg(root)
            .status()
            .with_context(|| format!("git clone {git_url}"))?;
        if !status.success() {
            bail!("git clone failed: {git_url} (exit {:?})", status.code());
        }
        Ok(())
    }
}

fn refresh_marketplace_dir(dir: &Path) -> Result<()> {
    if !dir.join(".git").exists() {
        bail!("marketplace clone missing at {}", dir.display());
    }
    let fetch = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["fetch", "--all"])
        .status()
        .with_context(|| format!("git fetch in {}", dir.display()))?;
    if !fetch.success() {
        bail!("git fetch failed for {}", dir.display());
    }
    let reset = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["reset", "--hard", "FETCH_HEAD"])
        .status()
        .with_context(|| format!("git reset in {}", dir.display()))?;
    if !reset.success() {
        bail!("git reset failed for {}", dir.display());
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

fn git_rev_parse_head(dir: &Path) -> String {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_default()
}

/// Shallow-clone `url` (optionally at `git_ref`) into `dest`. A `git_ref`
/// that is a commit SHA cannot go through `clone --branch` (git only accepts
/// branch/tag names there): clone the default branch shallow, then fetch and
/// check out the SHA — falling back to a full fetch when the server refuses
/// shallow SHA fetches.
fn git_clone_shallow(url: &str, git_ref: Option<&str>, dest: &Path) -> Result<()> {
    let sha_ref = git_ref.filter(|git_ref| is_commit_sha(git_ref));
    let mut cmd = Command::new("git");
    cmd.args(["clone", "--depth", "1", "--quiet"]);
    if let Some(git_ref) = git_ref.filter(|_| sha_ref.is_none()) {
        cmd.arg("--branch").arg(git_ref);
    }
    let status = cmd
        .arg(url)
        .arg(dest)
        .status()
        .with_context(|| format!("git clone {url}"))?;
    if !status.success() {
        bail!("git clone failed: {url} (exit {:?})", status.code());
    }
    if let Some(sha) = sha_ref {
        git_checkout_sha(dest, url, sha)?;
    }
    Ok(())
}

/// Check out one commit SHA in an existing clone: fetch it shallowly, then
/// check out FETCH_HEAD. Servers that refuse shallow SHA fetches get a full
/// fetch fallback. The marketplace pin is authoritative — after this, HEAD
/// is the pinned commit, not the remote's current default branch.
fn git_checkout_sha(dir: &Path, url: &str, sha: &str) -> Result<()> {
    let fetched = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["fetch", "--depth", "1", "origin", sha])
        .status()
        .with_context(|| format!("git fetch {sha} in {url}"))?;
    if !fetched.success() {
        let full = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["fetch", "origin"])
            .status()
            .with_context(|| format!("git fetch in {url}"))?;
        if !full.success() {
            bail!("git fetch failed for {url}");
        }
    }
    let checkout = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["checkout", "--quiet", "FETCH_HEAD"])
        .status()
        .with_context(|| format!("git checkout {sha} in {url}"))?;
    if !checkout.success() {
        bail!("git checkout failed for {sha} in {url}");
    }
    Ok(())
}

/// A 40-hex-digit ref is a commit SHA, not a branch or tag name — the shape
/// marketplace indexes pin provenance with.
fn is_commit_sha(git_ref: &str) -> bool {
    git_ref.len() == 40 && git_ref.chars().all(|c| c.is_ascii_hexdigit())
}

/// Materialize a marketplace entry's source into a local directory and
/// return the tree plus the git commit it came from. In-repo relative paths
/// are used directly (the marketplace clone is already a checkout); remote
/// sources are shallow-cloned into `scratch`, which the CALLER owns and must
/// keep alive until the tree has been consumed — a clone dir dropped at
/// function exit is exactly the bug that made every remote-source install
/// copy from a deleted directory. A pinned `sha` is checked out (fetch +
/// checkout, with a full-fetch fallback); the pin is authoritative — the
/// installed commit is the pinned one, not the remote's current HEAD.
fn materialize_source(
    repo_root: &Path,
    source: &MarketplacePluginSource,
    scratch: &Path,
) -> Result<(PathBuf, String)> {
    match source {
        MarketplacePluginSource::Relative(rel) => {
            if !is_safe_subpath(rel) {
                bail!("plugin source {rel:?} escapes the marketplace clone");
            }
            let tree = repo_root.join(strip_source_prefix(rel));
            if !tree.exists() {
                bail!("plugin source {} missing", tree.display());
            }
            Ok((tree, git_rev_parse_head(repo_root)))
        }
        MarketplacePluginSource::Unsupported { kind } => {
            bail!(
                "unsupported marketplace source kind {kind:?} — cannot install this plugin \
                 (manox may be older than the marketplace)"
            )
        }
        MarketplacePluginSource::Remote(remote) => {
            let (url, subpath, git_ref, pinned_sha) = match remote {
                RemoteSource::GitHub { repo, r#ref, sha } => (
                    format!("https://github.com/{repo}.git"),
                    None,
                    r#ref.as_deref(),
                    sha.as_deref(),
                ),
                RemoteSource::GitSubdir {
                    url,
                    path,
                    r#ref,
                    sha,
                } => (
                    url.clone(),
                    path.as_deref(),
                    r#ref.as_deref(),
                    sha.as_deref(),
                ),
            };
            if let Some(sub) = subpath
                && !is_safe_subpath(sub)
            {
                bail!("plugin subpath {sub:?} escapes the cloned repository");
            }
            let clone_dir = scratch.join("clone");
            git_clone_shallow(&url, git_ref, &clone_dir)?;
            if let Some(sha) = pinned_sha {
                git_checkout_sha(&clone_dir, &url, sha)?;
            }
            let tree = match subpath {
                Some(sub) => clone_dir.join(sub.trim_matches('/')),
                None => clone_dir.clone(),
            };
            if !tree.exists() {
                bail!("plugin source {} missing in {url}", tree.display());
            }
            // Provenance is the pin when there is one — the tree IS that
            // commit after the checkout above.
            let commit = pinned_sha
                .map(str::to_string)
                .unwrap_or_else(|| git_rev_parse_head(&clone_dir));
            Ok((tree, commit))
        }
    }
}

/// Reject marketplace-controlled path fragments that would escape the clone:
/// absolute paths and `..` components. `Path::join` with an absolute segment
/// replaces the base entirely, and `..` walks out of it — either turns a
/// marketplace index entry into "copy any directory on disk into the install
/// tree (and run its hooks)". A leading `.` is harmless (Rust keeps it as a
/// `CurDir` component; joining is a no-op) and allowed.
fn is_safe_subpath(path: &str) -> bool {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() || trimmed.starts_with('/') || trimmed.starts_with('~') {
        return false;
    }
    std::path::Path::new(trimmed).components().all(|c| {
        matches!(
            c,
            std::path::Component::Normal(_) | std::path::Component::CurDir
        )
    })
}

/// The `./` prefix marketplace indexes put on relative sources.
fn strip_source_prefix(path: &str) -> &str {
    path.strip_prefix("./").unwrap_or(path)
}

/// Best-effort `.in_use/<pid>` marker so Claude Code's cache GC cannot delete
/// a tree manox is running from; dead pids are pruned by Claude Code itself,
/// so no exit cleanup is needed.
fn mark_in_use(root: &Path) {
    let marker = root.join(".in_use");
    let pid = std::process::id().to_string();
    if marker.join(&pid).exists() {
        return;
    }
    if let Err(err) =
        std::fs::create_dir_all(&marker).and_then(|()| std::fs::write(marker.join(pid), b""))
    {
        tracing::debug!(
            "could not write .in_use marker under {}: {err}",
            root.display()
        );
    }
}

// ---------------------------------------------------------------------------
// Plugin manifests
// ---------------------------------------------------------------------------

/// Parsed plugin `plugin.json` — minimal metadata. Extra fields are ignored
/// on read (the manifest format is Claude Code's; manox only needs identity).
#[derive(Debug, Clone, Deserialize)]
pub struct PluginManifest {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
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

#[cfg(test)]
mod tests {
    use super::*;

    const INDEX_RAW: &str = r#"{
        "$schema": "https://example.com/schema.json",
        "name": "demo",
        "owner": {"name": "demo-owner"},
        "description": "d",
        "plugins": [
            {"name": "gitwork", "description": "g", "source": "./plugins/gitwork"},
            {"name": "remote-gh", "source": {"source": "github", "repo": "dspo/remote-gh"}},
            {"name": "remote-sub", "source": {"source": "git-subdir", "url": "https://example.com/x.git", "path": "plugins/remote-sub", "ref": "v1.2.3", "sha": "abc123"}},
            {"name": "url-kind", "source": {"source": "url", "url": "https://example.com/repo.git", "sha": "d645d2c8ce0689a568224436061872ab9f0ab179"}},
            {"name": "alien", "source": {"source": "svn", "url": "https://example.com/svn"}}
        ]
    }"#;

    /// Redirect the Claude home into a temp dir for the duration of one test.
    /// Holds the shared `MANOX_CLAUDE_HOME` lock for its whole lifetime —
    /// dropping the lock inside `new` would let a sibling test repoint the
    /// env mid-test — and unsets the var while still holding it.
    struct ClaudeHome {
        _guard: std::sync::MutexGuard<'static, ()>,
        _dir: tempfile::TempDir,
    }

    impl ClaudeHome {
        fn new() -> Self {
            let guard = crate::paths::tests::claude_home_lock();
            let dir = tempfile::tempdir().unwrap();
            // SAFETY: serialized by the guard held below for this test's life.
            unsafe { std::env::set_var("MANOX_CLAUDE_HOME", dir.path()) };
            Self {
                _guard: guard,
                _dir: dir,
            }
        }
    }

    impl Drop for ClaudeHome {
        fn drop(&mut self) {
            // SAFETY: `_guard` still holds the lock; fields drop after this
            // body, so no sibling test can observe the intermediate state.
            unsafe { std::env::remove_var("MANOX_CLAUDE_HOME") };
        }
    }

    fn write_index(root: &Path, body: &str) {
        let dir = root.join(".claude-plugin");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("marketplace.json"), body).unwrap();
    }

    #[test]
    fn parses_marketplace_index_with_all_source_shapes() {
        let idx: MarketplaceIndex = serde_json::from_str(INDEX_RAW).unwrap();
        assert_eq!(idx.name, "demo");
        assert_eq!(idx.plugins.len(), 5);
        assert!(
            matches!(&idx.plugins[0].source, MarketplacePluginSource::Relative(path) if path == "./plugins/gitwork")
        );
        assert!(
            matches!(&idx.plugins[1].source, MarketplacePluginSource::Remote(RemoteSource::GitHub { repo, .. }) if repo == "dspo/remote-gh")
        );
        assert!(
            matches!(&idx.plugins[2].source, MarketplacePluginSource::Remote(RemoteSource::GitSubdir { url, path, r#ref, sha }) if url == "https://example.com/x.git" && path.as_deref() == Some("plugins/remote-sub") && r#ref.as_deref() == Some("v1.2.3") && sha.as_deref() == Some("abc123"))
        );
        assert_eq!(
            idx.plugins[2].source.display(),
            "git-subdir:https://example.com/x.git@v1.2.3"
        );
        // `url` kind (104/203 entries of the official index): a whole-repo
        // clone pinned by sha — a path-less git-subdir.
        assert!(
            matches!(
                &idx.plugins[3].source,
                MarketplacePluginSource::Remote(RemoteSource::GitSubdir { url, path, sha: Some(sha), .. })
                    if url == "https://example.com/repo.git" && path.is_none() && sha == "d645d2c8ce0689a568224436061872ab9f0ab179"
            ),
            "url kind must deserialize into a sha-pinned path-less clone, got {:?}",
            idx.plugins[3].source
        );
        // An unknown kind must not poison the entry, let alone the index.
        assert!(matches!(
            &idx.plugins[4].source,
            MarketplacePluginSource::Unsupported { kind } if kind == "svn"
        ));
        assert!(!idx.plugins[4].source.installable());
        assert_eq!(idx.plugins[4].source.display(), "unsupported:svn");
    }

    #[test]
    fn malformed_sources_degrade_to_unsupported_not_error() {
        for raw in [
            r#"{"source": "svn", "url": "x"}"#,
            r#"{"source": "github"}"#,
            r#"{"source": "git-subdir"}"#,
            r#"{"source": "url"}"#,
            r#"{}"#,
            r#"7"#,
        ] {
            let parsed: MarketplacePluginSource = serde_json::from_str(raw).unwrap();
            assert!(
                matches!(parsed, MarketplacePluginSource::Unsupported { .. }),
                "{raw} must degrade to Unsupported, got {parsed:?}"
            );
            assert!(!parsed.installable());
        }
    }

    #[test]
    fn parses_plugin_manifest() {
        let raw = r#"{"name":"mimo","description":"x","author":{"name":"a"},"version":"0.2.0"}"#;
        let m: PluginManifest = serde_json::from_str(raw).unwrap();
        assert_eq!(m.name, "mimo");
        assert_eq!(m.version.as_deref(), Some("0.2.0"));
    }

    #[test]
    fn normalize_git_url_strips_suffixes() {
        assert_eq!(
            normalize_git_url("https://github.com/dspo/agent-marketplace.git"),
            normalize_git_url("https://github.com/dspo/agent-marketplace")
        );
        assert_eq!(normalize_git_url("https://x.y/z/"), "https://x.y/z");
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

    /// Seed a local git marketplace with one plugin; return (dir, url, slug).
    fn seed_marketplace() -> (tempfile::TempDir, String, String) {
        let market = tempfile::tempdir().unwrap();
        let plugin_src = market.path().join("plugins/gitwork");
        std::fs::create_dir_all(plugin_src.join(".claude-plugin")).unwrap();
        std::fs::create_dir_all(plugin_src.join("skills/review")).unwrap();
        std::fs::write(
            plugin_src.join(".claude-plugin/plugin.json"),
            r#"{"name":"gitwork","description":"g","version":"3.0.0"}"#,
        )
        .unwrap();
        std::fs::write(
            plugin_src.join("skills/review/SKILL.md"),
            "---\nname: review\ndescription: r\n---\nbody",
        )
        .unwrap();
        write_index(
            market.path(),
            r#"{"name":"fixture","plugins":[{"name":"gitwork","description":"g","source":"./plugins/gitwork"}]}"#,
        );
        for args in [
            vec!["init", "-q"],
            vec!["-c", "user.email=t@t", "-c", "user.name=t", "add", "."],
            vec![
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-qm",
                "init",
            ],
        ] {
            let status = Command::new("git")
                .arg("-C")
                .arg(market.path())
                .args(&args)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        }
        let url = market.path().to_str().unwrap().to_string();
        let slug = paths::marketplace_slug(&url);
        (market, url, slug)
    }

    /// Seed `installed_plugins.json` with (key, existing tree, scope)
    /// entries and `settings.json` with `enabledPlugins: {}`.
    fn seed_registry(entries: &[(&str, &Path, &str)]) {
        let registry_path = paths::installed_plugins_file().unwrap();
        std::fs::create_dir_all(registry_path.parent().unwrap()).unwrap();
        let mut plugins = serde_json::Map::new();
        for (key, tree, scope) in entries {
            plugins.insert(
                (*key).to_string(),
                json!([{
                    "scope": scope,
                    "installPath": tree,
                    "version": "1.0.0",
                    "installedAt": "t0",
                    "lastUpdated": "t0",
                    "gitCommitSha": "s",
                }]),
            );
        }
        std::fs::write(
            &registry_path,
            serde_json::to_string(&json!({"version": 2, "plugins": plugins})).unwrap(),
        )
        .unwrap();
        let settings = paths::claude_settings_file().unwrap();
        std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
        std::fs::write(&settings, r#"{"enabledPlugins":{}}"#).unwrap();
    }

    /// Add explicit enabledPlugins toggles on top of [`seed_registry`].
    fn seed_toggles(toggles: &[(&str, bool)]) {
        let path = paths::claude_settings_file().unwrap();
        let mut doc = read_json(&path).unwrap().unwrap();
        let obj = doc.as_object_mut().unwrap();
        let map = obj
            .get_mut("enabledPlugins")
            .unwrap()
            .as_object_mut()
            .unwrap();
        for (key, enabled) in toggles {
            map.insert((*key).to_string(), json!(enabled));
        }
        std::fs::write(&path, serde_json::to_string(&doc).unwrap()).unwrap();
    }

    /// Full lifecycle against a real local git marketplace, redirected into a
    /// temp Claude home: install writes the cache tree + both registries;
    /// disable/enable flip the settings toggle; uninstall removes everything;
    /// an unrelated settings key survives every write.
    #[test]
    fn install_enable_disable_uninstall_lifecycle() {
        let _home = ClaudeHome::new();
        std::fs::create_dir_all(paths::claude_home().unwrap()).unwrap();
        std::fs::write(
            paths::claude_settings_file().unwrap(),
            r#"{"model":"deepseek-v4-pro[1m]","enabledPlugins":{}}"#,
        )
        .unwrap();
        let (_market, url, slug) = seed_marketplace();

        PluginManager::add_marketplace(&url).unwrap();
        let known = read_json(&paths::known_marketplaces_file().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(
            known[slug.as_str()]["source"]["url"].as_str(),
            Some(url.as_str())
        );

        PluginManager::install(&slug, "gitwork").unwrap();
        let key = format!("gitwork@{slug}");
        let registry = read_json(&paths::installed_plugins_file().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(registry["version"], json!(2));
        let entry = &registry["plugins"][key.as_str()][0];
        let root = PathBuf::from(entry["installPath"].as_str().unwrap());
        assert!(root.join(".claude-plugin").join("plugin.json").exists());
        assert_eq!(entry["version"], json!("3.0.0"));
        assert!(!entry["gitCommitSha"].as_str().unwrap().is_empty());

        let settings = read_json(&paths::claude_settings_file().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(settings["enabledPlugins"][key.as_str()], json!(true));
        // The unrelated key survived the read-modify-write.
        assert_eq!(settings["model"], json!("deepseek-v4-pro[1m]"));

        let installed = PluginManager::installed();
        assert_eq!(installed.len(), 1);
        assert_eq!(installed[0].name, "gitwork");
        assert_eq!(installed[0].marketplace, slug);
        assert_eq!(installed[0].root, root);

        PluginManager::disable("gitwork").unwrap();
        assert!(PluginManager::installed().is_empty());
        let details = PluginManager::installed_details();
        assert_eq!(details.len(), 1);
        assert!(!details[0].enabled);
        assert_eq!(details[0].version.as_deref(), Some("3.0.0"));

        PluginManager::enable("gitwork").unwrap();
        assert_eq!(PluginManager::installed().len(), 1);

        PluginManager::uninstall("gitwork").unwrap();
        assert!(PluginManager::all_installed().is_empty());
        let registry = read_json(&paths::installed_plugins_file().unwrap())
            .unwrap()
            .unwrap();
        assert!(registry["plugins"].as_object().unwrap().is_empty());
        let settings = read_json(&paths::claude_settings_file().unwrap())
            .unwrap()
            .unwrap();
        assert!(settings["enabledPlugins"].as_object().unwrap().is_empty());
        assert!(!root.exists());
    }

    /// A registry stamped with a newer schema version is still readable but
    /// refuses writes.
    #[test]
    fn newer_registry_schema_degrades_to_read_only() {
        let _home = ClaudeHome::new();
        let tree = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tree.path().join(".claude-plugin")).unwrap();
        std::fs::write(
            tree.path().join(".claude-plugin/plugin.json"),
            r#"{"name":"gitwork","version":"9.9.9"}"#,
        )
        .unwrap();
        let registry_path = paths::installed_plugins_file().unwrap();
        std::fs::create_dir_all(registry_path.parent().unwrap()).unwrap();
        std::fs::write(
            &registry_path,
            format!(
                r#"{{"version":3,"plugins":{{"gitwork@m":[{{"scope":"user","installPath":{},"version":"9.9.9","installedAt":"t0","lastUpdated":"t0","gitCommitSha":"s"}}]}}}}"#,
                serde_json::to_string(tree.path().to_str().unwrap()).unwrap()
            ),
        )
        .unwrap();

        assert_eq!(PluginManager::all_installed().len(), 1);
        let err = InstalledPlugins::record_install("x", "m", Path::new("/tmp/x"), "1.0.0", "s")
            .unwrap_err();
        assert!(err.to_string().contains("newer than supported"));
    }

    /// An unparseable registry is never replaced by a fresh document.
    #[test]
    fn unparseable_registry_aborts_writes() {
        let _home = ClaudeHome::new();
        let registry_path = paths::installed_plugins_file().unwrap();
        std::fs::create_dir_all(registry_path.parent().unwrap()).unwrap();
        std::fs::write(&registry_path, "{not json").unwrap();
        assert!(
            InstalledPlugins::record_install("x", "m", Path::new("/tmp/x"), "1.0.0", "s").is_err()
        );
        assert_eq!(
            std::fs::read_to_string(&registry_path).unwrap(),
            "{not json"
        );
    }

    /// A URL Claude Code already registered under its own slug is never
    /// registered twice — the known slug and clone are reused.
    #[test]
    fn add_marketplace_reuses_known_slug_for_same_url() {
        let _home = ClaudeHome::new();
        let (_market, url, slug) = seed_marketplace();
        // Pre-register the same URL under a foreign slug, as Claude Code
        // would have done.
        let foreign_root = paths::plugins_dir().unwrap().join("marketplaces/remora");
        KnownMarketplaces::register("remora", &url, &foreign_root).unwrap();

        PluginManager::add_marketplace(&url).unwrap();

        let known = read_json(&paths::known_marketplaces_file().unwrap())
            .unwrap()
            .unwrap();
        let keys = known.as_object().unwrap();
        assert!(keys.contains_key("remora"));
        assert!(
            !keys.contains_key(slug.as_str()),
            "same URL must not gain a second registration"
        );
        assert!(foreign_root.join(".git").exists());
    }

    /// Toggles are keyed `name@marketplace`: disabling `foo@m1` must not
    /// keep `foo@m1` scanned merely because `foo@m2` exists (the bare-name
    /// filter leaked key-level state across marketplaces).
    #[test]
    fn toggles_are_key_scoped() {
        let _home = ClaudeHome::new();
        let tree1 = tempfile::tempdir().unwrap();
        let tree2 = tempfile::tempdir().unwrap();
        for tree in [&tree1, &tree2] {
            std::fs::create_dir_all(tree.path().join(".claude-plugin")).unwrap();
        }
        seed_registry(&[
            ("foo@m1", tree1.path(), "user"),
            ("foo@m2", tree2.path(), "user"),
        ]);
        seed_toggles(&[("foo@m1", false)]);

        let installed = PluginManager::installed();
        assert_eq!(
            installed.iter().map(|p| p.key.as_str()).collect::<Vec<_>>(),
            ["foo@m2"],
            "the explicitly disabled foo@m1 must not be scanned"
        );
        let details = PluginManager::installed_details();
        assert_eq!(details.len(), 2);
        let m1 = details.iter().find(|d| d.marketplace == "m1").unwrap();
        let m2 = details.iter().find(|d| d.marketplace == "m2").unwrap();
        assert!(!m1.enabled);
        assert!(m2.enabled);
        assert_eq!(PluginManager::all_installed().len(), 2);
    }

    /// A project/local-scope entry recorded by Claude Code under the same
    /// key survives a manox (re)install — only the user-scope entry is
    /// superseded.
    #[test]
    fn record_install_preserves_sibling_scopes() {
        let _home = ClaudeHome::new();
        let project_tree = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(project_tree.path().join(".claude-plugin")).unwrap();
        seed_registry(&[]);
        let registry_path = paths::installed_plugins_file().unwrap();
        std::fs::write(
            &registry_path,
            format!(
                r#"{{"version":2,"plugins":{{"x@m":[{{"scope":"project","installPath":{},"version":"0.1.0","installedAt":"t0","lastUpdated":"t0","gitCommitSha":"s"}}]}}}}"#,
                serde_json::to_string(project_tree.path().to_str().unwrap()).unwrap()
            ),
        )
        .unwrap();

        let user_tree = tempfile::tempdir().unwrap();
        InstalledPlugins::record_install("x", "m", user_tree.path(), "1.0.0", "abc").unwrap();

        let registry = read_json(&registry_path).unwrap().unwrap();
        let entries = registry["plugins"]["x@m"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["scope"], json!("project"));
        assert_eq!(entries[0]["version"], json!("0.1.0"));
        assert_eq!(entries[1]["scope"], json!("user"));
        assert_eq!(entries[1]["version"], json!("1.0.0"));
    }

    /// A key whose entries are all project-scope is Claude Code's per-repo
    /// bookkeeping — the user-level loaders must not promote it into every
    /// session.
    #[test]
    fn project_only_keys_are_not_scanned() {
        let _home = ClaudeHome::new();
        let tree = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tree.path().join(".claude-plugin")).unwrap();
        seed_registry(&[("repo@m", tree.path(), "project")]);

        assert!(PluginManager::all_installed().is_empty());
        assert!(PluginManager::installed().is_empty());
    }

    /// A `git-subdir` entry whose `ref` pins a commit SHA materializes via
    /// fetch + checkout (clone --branch only accepts branch/tag names).
    #[test]
    fn materializes_a_sha_pinned_remote_source() {
        let repo = tempfile::tempdir().unwrap();
        let plugins = repo.path().join("plugins/pinned");
        std::fs::create_dir_all(&plugins).unwrap();
        std::fs::write(plugins.join("marker.txt"), "pinned-content").unwrap();
        for args in [
            vec!["init", "-q"],
            vec!["-c", "user.email=t@t", "-c", "user.name=t", "add", "."],
            vec![
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-qm",
                "init",
            ],
        ] {
            let status = Command::new("git")
                .arg("-C")
                .arg(repo.path())
                .args(&args)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        }
        let sha = git_rev_parse_head(repo.path());
        assert!(is_commit_sha(&sha));

        let source = MarketplacePluginSource::Remote(RemoteSource::GitSubdir {
            url: repo.path().to_str().unwrap().to_string(),
            path: Some("plugins/pinned".to_string()),
            r#ref: Some(sha.clone()),
            sha: None,
        });
        let scratch = tempfile::tempdir().unwrap();
        let (tree, provenance) =
            materialize_source(Path::new("/nonexistent"), &source, scratch.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(tree.join("marker.txt")).unwrap(),
            "pinned-content"
        );
        assert_eq!(provenance, sha);
    }

    /// Registering a URL Claude Code already recorded merges into the
    /// record: the CC-authored source shape and any extra fields survive;
    /// only installLocation/lastUpdated refresh.
    #[test]
    fn register_merges_into_an_existing_claude_code_record() {
        let _home = ClaudeHome::new();
        let (_market, url, _slug) = seed_marketplace();
        let foreign_root = paths::plugins_dir().unwrap().join("marketplaces/remora");
        // Claude Code's registration for this URL: a `name` field manox
        // never models, ahead of the manox merge.
        let path = paths::known_marketplaces_file().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            format!(
                r#"{{"remora":{{"extraField":true,"name":"remora-plugins","source":{{"source":"git","url":{}}},"installLocation":{},"lastUpdated":"t0"}}}}"#,
                serde_json::to_string(url.as_str()).unwrap(),
                serde_json::to_string(foreign_root.to_str().unwrap()).unwrap()
            ),
        )
        .unwrap();

        PluginManager::add_marketplace(&url).unwrap();

        let known = read_json(&path).unwrap().unwrap();
        let record = &known["remora"];
        assert_eq!(record["extraField"], json!(true), "unknown fields survive");
        assert_eq!(
            record["name"],
            json!("remora-plugins"),
            "unknown fields survive"
        );
        assert_eq!(
            record["source"]["source"],
            json!("git"),
            "a CC-authored source shape must not be rewritten"
        );
        assert_ne!(record["lastUpdated"], json!("t0"));
        // The clone landed in the recorded location, not a slug-derived one.
        assert!(foreign_root.join(".git").exists());
    }

    /// Claude Code omits `gitCommitSha` (and can omit the timestamps) when
    /// the marketplace entry carries no sha — observed on the real
    /// huggingface-skills install. Requiring those fields made the installed
    /// plugin silently invisible: the entry failed to parse and was skipped.
    #[test]
    fn install_entries_without_provenance_fields_stay_visible() {
        let _home = ClaudeHome::new();
        let tree = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tree.path().join(".claude-plugin")).unwrap();
        let registry_path = paths::installed_plugins_file().unwrap();
        std::fs::create_dir_all(registry_path.parent().unwrap()).unwrap();
        std::fs::write(
            &registry_path,
            format!(
                r#"{{"version":2,"plugins":{{"huggingface-skills@claude-plugins-official":[{{"scope":"user","installPath":{},"version":"1.0.2","installedAt":"2026-09-30T04:01:38.828Z","lastUpdated":"2026-09-30T04:01:38.828Z"}}]}}}}"#,
                serde_json::to_string(tree.path().to_str().unwrap()).unwrap()
            ),
        )
        .unwrap();

        let installed = PluginManager::all_installed();
        assert_eq!(installed.len(), 1, "a sha-less install must stay visible");
        assert_eq!(
            installed[0].name, "huggingface-skills",
            "the real-world regression shape"
        );
        assert_eq!(PluginManager::installed().len(), 1);
    }

    /// Marketplace-controlled paths may not escape the clone: `Path::join`
    /// with an absolute segment replaces the base, `..` walks out — either
    /// would let an index entry copy any directory on disk into the install
    /// tree, hooks included.
    #[test]
    fn source_paths_may_not_escape_the_clone() {
        let market = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(market.path().join("plugins/ok/.claude-plugin")).unwrap();
        std::fs::write(
            market.path().join("plugins/ok/.claude-plugin/plugin.json"),
            r#"{"name":"ok"}"#,
        )
        .unwrap();

        for source in [
            MarketplacePluginSource::Relative("../escape".to_string()),
            MarketplacePluginSource::Relative("/etc".to_string()),
        ] {
            let scratch = tempfile::tempdir().unwrap();
            let err = materialize_source(market.path(), &source, scratch.path()).unwrap_err();
            assert!(
                err.to_string().contains("escapes"),
                "{source:?} must be refused: {err:#}"
            );
        }

        let remote = MarketplacePluginSource::Remote(RemoteSource::GitSubdir {
            url: market.path().to_str().unwrap().to_string(),
            path: Some("../../../etc".to_string()),
            r#ref: None,
            sha: None,
        });
        let scratch = tempfile::tempdir().unwrap();
        let err = materialize_source(market.path(), &remote, scratch.path()).unwrap_err();
        assert!(err.to_string().contains("escapes"), "{err:#}");
    }

    /// A sha-only entry (no `ref` — 107/203 of the official index) must
    /// install the pinned commit, not the remote's current HEAD.
    #[test]
    fn materializes_a_sha_only_remote_source_at_the_pin() {
        let repo = tempfile::tempdir().unwrap();
        let plugins = repo.path().join("plugins/pinned");
        std::fs::create_dir_all(&plugins).unwrap();
        std::fs::write(plugins.join("marker.txt"), "pinned-content").unwrap();
        for args in [
            vec!["init", "-q"],
            vec!["-c", "user.email=t@t", "-c", "user.name=t", "add", "."],
            vec![
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-qm",
                "init",
            ],
        ] {
            let status = Command::new("git")
                .arg("-C")
                .arg(repo.path())
                .args(&args)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        }
        let pinned_sha = git_rev_parse_head(repo.path());
        // A second commit moves HEAD away from the pin.
        std::fs::write(plugins.join("marker.txt"), "head-content").unwrap();
        for args in [
            vec!["-c", "user.email=t@t", "-c", "user.name=t", "add", "."],
            vec![
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-qm",
                "second",
            ],
        ] {
            let status = Command::new("git")
                .arg("-C")
                .arg(repo.path())
                .args(&args)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        }
        assert_ne!(git_rev_parse_head(repo.path()), pinned_sha);

        let source = MarketplacePluginSource::Remote(RemoteSource::GitSubdir {
            url: repo.path().to_str().unwrap().to_string(),
            path: Some("plugins/pinned".to_string()),
            r#ref: None,
            sha: Some(pinned_sha.clone()),
        });
        let scratch = tempfile::tempdir().unwrap();
        let (tree, provenance) =
            materialize_source(Path::new("/nonexistent"), &source, scratch.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(tree.join("marker.txt")).unwrap(),
            "pinned-content",
            "the tree must be the pinned commit, not HEAD"
        );
        assert_eq!(provenance, pinned_sha);
    }

    /// Uninstall removes only the user-scope tree and entry: a project-scope
    /// sibling Claude Code recorded under the same key — and its toggle —
    /// survive, mirroring what `record_install` protects on the write side.
    #[test]
    fn uninstall_preserves_sibling_scopes() {
        let _home = ClaudeHome::new();
        let user_tree = tempfile::tempdir().unwrap();
        let project_tree = tempfile::tempdir().unwrap();
        for tree in [&user_tree, &project_tree] {
            std::fs::create_dir_all(tree.path().join(".claude-plugin")).unwrap();
        }
        seed_registry(&[]);
        let registry_path = paths::installed_plugins_file().unwrap();
        std::fs::write(
            &registry_path,
            format!(
                r#"{{"version":2,"plugins":{{"x@m":[{{"scope":"project","installPath":{},"version":"0.1.0","installedAt":"t0","lastUpdated":"t0","gitCommitSha":"s"}},{{"scope":"user","installPath":{},"version":"1.0.0","installedAt":"t1","lastUpdated":"t1","gitCommitSha":"t"}}]}}}}"#,
                serde_json::to_string(project_tree.path().to_str().unwrap()).unwrap(),
                serde_json::to_string(user_tree.path().to_str().unwrap()).unwrap()
            ),
        )
        .unwrap();
        seed_toggles(&[("x@m", true)]);

        PluginManager::uninstall("x").unwrap();

        assert!(!user_tree.path().exists(), "the user tree goes");
        assert!(project_tree.path().exists(), "the project tree stays");
        let registry = read_json(&registry_path).unwrap().unwrap();
        let entries = registry["plugins"]["x@m"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["scope"], json!("project"));
        let settings = read_json(&paths::claude_settings_file().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(
            settings["enabledPlugins"]["x@m"],
            json!(true),
            "the toggle belongs to the surviving key"
        );

        // Uninstalling again with no user entry left is a no-op that keeps
        // the sibling bookkeeping.
        PluginManager::uninstall("x").unwrap();
        let registry = read_json(&registry_path).unwrap().unwrap();
        assert_eq!(registry["plugins"]["x@m"].as_array().unwrap().len(), 1);
    }

    /// Read-only probe of a real Claude Code installation: registry files,
    /// marketplace indexes, and the enabled set must all parse. Gated behind
    /// `MANOX_RUN_LIVE=1` like the other live tests; skips silently when no
    /// plugin store exists. This is the early-warning canary for Claude Code
    /// schema drift.
    #[test]
    fn live_shared_claude_home_parses() {
        if std::env::var("MANOX_RUN_LIVE").ok().as_deref() != Some("1") {
            return;
        }
        if paths::installed_plugins_file()
            .map(|p| !p.exists())
            .unwrap_or(true)
        {
            return;
        }
        let records = PluginManager::list_marketplace_records();
        assert!(
            !records.is_empty(),
            "live ~/.claude has a plugin store but no parseable marketplace"
        );
        let details = PluginManager::installed_details();
        assert!(
            !details.is_empty(),
            "live ~/.claude has a plugin store but no parseable install"
        );
        assert!(
            !PluginManager::installed().is_empty(),
            "live ~/.claude has installs but none enabled"
        );
        // Official-index drift probe: every entry of the largest marketplace
        // must parse. One unparseable entry used to erase the whole
        // marketplace from the UI (the url-kind mass, 104/203 at
        // 2026-09); this fails loudly the day a new kind appears.
        if let Some(record) = records.iter().max_by_key(|r| r.plugin_count) {
            let index = PluginManager::load_marketplace_index(&record.root).unwrap_or_else(|e| {
                panic!("largest marketplace {} unparseable: {e:#}", record.slug)
            });
            let raw_count = read_json(&record.root.join(".claude-plugin").join("marketplace.json"))
                .ok()
                .flatten()
                .and_then(|doc| doc.get("plugins").and_then(Value::as_array).cloned())
                .map(|a| a.len());
            if let Some(raw_count) = raw_count {
                assert_eq!(
                    index.plugins.len(),
                    raw_count,
                    "marketplace {}: {raw_count} raw entries but only {} parse —                      entries are being dropped, treat as schema drift",
                    record.slug,
                    index.plugins.len()
                );
            }
            let unsupported: Vec<_> = index
                .plugins
                .iter()
                .filter(|p| !p.source.installable())
                .collect();
            assert!(
                unsupported.is_empty(),
                "marketplace {}: {} entries use source kinds manox does not install: {:?}",
                record.slug,
                unsupported.len(),
                unsupported
                    .iter()
                    .map(|p| p.source.display())
                    .take(5)
                    .collect::<Vec<_>>()
            );
        }
    }
}
