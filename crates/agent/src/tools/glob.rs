//! `glob` tool: gitignore-aware file-path glob match (the `ignore` crate walk
//! + `globset` pattern match), capped and relaxable via flags.

use std::path::PathBuf;
use std::sync::Arc;

use globset::{Glob, GlobSetBuilder};
use gpui::{App, AppContext as _, Task};
use ignore::WalkBuilder;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::read_policy::ReadPolicy;
use crate::tool::AgentTool;

use super::{resolve_path, schema};

pub struct GlobTool {
    pub(crate) cwd: Arc<PathBuf>,
    pub(crate) read_policy: ReadPolicy,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct GlobInput {
    /// Glob pattern (e.g. `**/*.rs`), relative to `path`/cwd.
    /// Supports `{a,b}` alternation, `[ab]` classes, `**` recursion.
    /// `*.rs` matches top-level only; use `**/*.rs` for recursion.
    pattern: String,
    /// Search root, defaults to cwd.
    #[serde(default)]
    path: Option<String>,
    /// When true, ignore all `.gitignore`/`.ignore`/global gitignore rules.
    #[serde(default)]
    no_ignore: bool,
    /// When true, do not skip hidden (dot) files and directories.
    #[serde(default)]
    include_hidden: bool,
    /// When true, also yield directory entries that match the pattern.
    #[serde(default)]
    include_dirs: bool,
    /// Max results to return. Default 100. No hard ceiling — raising this
    /// risks blowing up your own context window with a huge repo; narrow
    /// `pattern` instead of raising `limit` when possible.
    #[serde(default)]
    limit: Option<usize>,
}

impl AgentTool for GlobTool {
    fn name(&self) -> &str {
        "Glob"
    }
    fn description(&self) -> &str {
        "Find file paths matching a glob pattern (relative to cwd). Honors .gitignore and skips hidden files by default; returns relative paths, capped at 100. Pass no_ignore/include_hidden/include_dirs/limit to relax. Note: limit has no hard ceiling — raising it can blow up the context, so prefer narrowing the pattern first."
    }
    fn input_schema(&self) -> serde_json::Value {
        schema::<GlobInput>()
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn run(
        &self,
        input: serde_json::Value,
        _cancel: CancellationToken,
        _ctx: &dyn crate::tool::ToolContext,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let parsed = match serde_json::from_value::<GlobInput>(input) {
            Ok(p) => p,
            Err(e) => {
                return cx.background_spawn(async move { Err(format!("input parse failed: {e}")) });
            }
        };
        let cwd = self.cwd.as_ref().clone();
        let read_policy = self.read_policy.clone();
        cx.background_spawn(async move {
            let root = parsed
                .path
                .map(|p| resolve_path(&p, &cwd))
                .unwrap_or_else(|| cwd.clone());
            // Deny a glob rooted in a sensitive subtree outright; prune such
            // subtrees during descent when rooted at a parent (e.g. `$HOME`).
            read_policy.check(&root)?;
            let prune_roots: Vec<PathBuf> = read_policy.denied_roots().to_vec();
            // Canonicalize the walk root so emitted paths share the prefix of
            // the canonicalized `prune_roots`; a symlinked root would otherwise
            // make `starts_with` miss sensitive subtrees.
            let walk_root = crate::sandbox::canonicalize_best_effort(&root);
            let matcher = GlobSetBuilder::new()
                .add(Glob::new(&parsed.pattern).map_err(|e| format!("glob invalid pattern: {e}"))?)
                .build()
                .map_err(|e| format!("glob build failed: {e}"))?;
            let limit = parsed.limit.unwrap_or(100);
            // Default flags enforce the strongest filtering; each toggle relaxes one axis.
            let mut builder = WalkBuilder::new(&walk_root);
            builder
                .hidden(!parsed.include_hidden)
                .ignore(!parsed.no_ignore)
                .git_ignore(!parsed.no_ignore)
                .git_global(!parsed.no_ignore)
                .git_exclude(!parsed.no_ignore)
                .parents(!parsed.no_ignore);
            if !prune_roots.is_empty() {
                builder.filter_entry(move |entry| {
                    let p = entry.path();
                    !prune_roots.iter().any(|r| p.starts_with(r))
                });
            }
            let mut out: Vec<String> = Vec::new();
            let mut truncated = false;
            for entry in builder.build().by_ref() {
                let Ok(e) = entry else { continue };
                let path = e.path();
                // Skip secret-named files in otherwise-permitted subtrees; the
                // prune filter only drops whole denied directories.
                if crate::read_policy::is_likely_secret_file(path) {
                    continue;
                }
                let is_dir = path.is_dir();
                if is_dir {
                    if !parsed.include_dirs {
                        continue;
                    }
                } else if !path.is_file() {
                    // Skip symlinks-to-nonfiles and special nodes.
                    continue;
                }
                let rel = path.strip_prefix(&walk_root).unwrap_or(path);
                if matcher.is_match(rel) {
                    out.push(rel.display().to_string());
                    if out.len() >= limit {
                        truncated = true;
                        break;
                    }
                }
            }
            out.sort();
            if out.is_empty() {
                Ok("No matching files".to_string())
            } else if truncated {
                // Count-based truncation (not byte), so it does not go through
                // `TruncatedText`; the wording is unified to the `⚠` advisory style.
                Ok(format!(
                    "⚠ Too many matches (showing first {limit}, more omitted). Narrow `pattern` or raise `limit` and retry — do not speculate about omitted matches.\n\n{}",
                    out.join("\n")
                ))
            } else {
                Ok(out.join("\n"))
            }
        })
    }
}
