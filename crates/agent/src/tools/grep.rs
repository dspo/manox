//! `grep` tool: in-process ripgrep search (grep-searcher + grep-regex + ignore),
//! returning `file:line:content` matches.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{App, AppContext as _, Task};
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{SearcherBuilder, Sink, SinkMatch};
use ignore::{WalkBuilder, overrides::OverrideBuilder};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::read_policy::ReadPolicy;
use crate::tool::AgentTool;

use super::{resolve_path, schema};

pub struct GrepTool {
    pub(crate) cwd: Arc<PathBuf>,
    pub(crate) read_policy: ReadPolicy,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct GrepInput {
    /// Regex pattern.
    pattern: String,
    /// Search root directory (defaults to cwd).
    #[serde(default)]
    path: Option<String>,
    /// Optional filename glob filter (e.g. `*.rs`).
    #[serde(default)]
    glob: Option<String>,
    /// Maximum matches to return (defaults to 200, hard-capped at 1000).
    #[serde(default)]
    limit: Option<u32>,
    /// Number of leading matches to skip, for paging through large result
    /// sets (defaults to 0).
    #[serde(default)]
    offset: Option<u32>,
}

impl AgentTool for GrepTool {
    fn name(&self) -> &str {
        super::GREP
    }
    fn description(&self) -> &str {
        "Search file contents by regex, returning matching lines (with line numbers). \
         Results are capped per line and by count; use limit/offset to page, or \
         narrow the search with path/glob/pattern when results are truncated."
    }
    fn input_schema(&self) -> serde_json::Value {
        schema::<GrepInput>()
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
        let parsed = match serde_json::from_value::<GrepInput>(input) {
            Ok(p) => p,
            Err(e) => {
                return cx.background_spawn(async move { Err(format!("input parse failed: {e}")) });
            }
        };
        let base = parsed
            .path
            .map(|p| resolve_path(&p, &self.cwd))
            .unwrap_or_else(|| self.cwd.as_ref().clone());
        let read_policy = self.read_policy.clone();
        cx.background_spawn(async move {
            run_grep(
                &parsed.pattern,
                &base,
                parsed.glob.as_deref(),
                parsed.limit,
                parsed.offset,
                &read_policy,
            )
        })
    }
}

/// Accumulates search results as "file:line:content" lines.
struct GrepSink {
    path: String,
    results: Vec<String>,
}

impl Sink for GrepSink {
    type Error = std::io::Error;

    fn matched(
        &mut self,
        _searcher: &grep_searcher::Searcher,
        mat: &SinkMatch<'_>,
    ) -> Result<bool, Self::Error> {
        let line = mat.line_number().unwrap_or(0);
        let content = String::from_utf8_lossy(mat.bytes());
        let content = content.trim_end();
        // A match on a minified/dumped mega-line (JSON blobs, bundled JS)
        // must not flood the result: cap the line, the total cap in
        // `truncate_result` stays a backstop rather than the only defense.
        let content = crate::tools::truncate::truncate_line(content);
        self.results
            .push(format!("{}:{}:{}", self.path, line, content));
        Ok(true)
    }
}

fn run_grep(
    pattern: &str,
    root: &Path,
    glob: Option<&str>,
    limit: Option<u32>,
    offset: Option<u32>,
    read_policy: &ReadPolicy,
) -> Result<String, String> {
    let limit = limit.unwrap_or(200).min(1000) as usize;
    let offset = offset.unwrap_or(0) as usize;
    // Deny a search rooted in a sensitive subtree outright.
    read_policy.check(root)?;
    // Canonicalized denied roots for walk pruning — a search rooted at a
    // parent (e.g. `$HOME`) must not descend into `~/.ssh` or `~/Library`.
    let prune_roots: Vec<PathBuf> = read_policy.denied_roots().to_vec();
    // Canonicalize the walk root so emitted paths share the prefix of the
    // canonicalized `prune_roots`. A symlinked root (e.g. `/var` →
    // `/private/var`) would otherwise make `starts_with` miss sensitive
    // subtrees and let the walker descend into them.
    let walk_root = crate::sandbox::canonicalize_best_effort(root);

    let matcher = RegexMatcherBuilder::new()
        .build(pattern)
        .map_err(|e| format!("grep invalid regex: {e}"))?;

    let mut walker = WalkBuilder::new(&walk_root);
    walker.standard_filters(true);
    walker.hidden(false);
    // Prune denied subtrees during descent. Returning false skips the entry and
    // its children; the root itself was already checked above, so this catches
    // sensitive dirs nested under the search root.
    if !prune_roots.is_empty() {
        walker.filter_entry(move |entry| {
            let p = entry.path();
            !prune_roots.iter().any(|r| p.starts_with(r))
        });
    }

    if let Some(g) = glob {
        let overrides = OverrideBuilder::new(&walk_root)
            .add(g)
            .map_err(|e| format!("grep invalid glob: {e}"))?
            .build()
            .map_err(|e| format!("grep glob build failed: {e}"))?;
        walker.overrides(overrides);
    }

    let mut searcher = SearcherBuilder::new()
        .line_number(true)
        .binary_detection(grep_searcher::BinaryDetection::quit(b'\x00'))
        .build();

    let mut all_results: Vec<String> = Vec::new();
    let mut seen = 0usize;
    let mut has_more = false;
    'walk: for result in walker.build() {
        let entry = match result {
            Ok(e) => e,
            Err(_) => continue,
        };
        let is_file = entry.file_type().map(|ft| ft.is_file()).unwrap_or(false);
        if !is_file {
            continue;
        }
        let file_path = entry.path();
        // The prune filter only drops whole denied directories; a secret-named
        // file nested in an otherwise-permitted subtree (e.g. a project-local
        // `.env`) would still be searched. Skip it by filename.
        if crate::read_policy::is_likely_secret_file(file_path) {
            continue;
        }
        let mut sink = GrepSink {
            path: file_path.display().to_string(),
            results: Vec::new(),
        };
        if searcher
            .search_path(&matcher, file_path, &mut sink)
            .is_err()
        {
            continue;
        }
        for r in sink.results {
            seen += 1;
            if seen <= offset {
                continue;
            }
            if all_results.len() >= limit {
                // One extra match past the page proves the page is partial;
                // stop the walk rather than counting the rest.
                has_more = true;
                break 'walk;
            }
            all_results.push(r);
        }
    }

    // Record hashline snapshots for matched files so the model can directly
    // edit_file without re-reading. Limited to 20 files to avoid excessive I/O.
    if !all_results.is_empty() {
        let mut seen_files = std::collections::HashSet::new();
        for result_line in &all_results {
            if let Some((file_path, _)) = result_line.split_once(':')
                && seen_files.len() < 20
            {
                seen_files.insert(file_path.to_string());
            }
        }
        let store = crate::hashline::global();
        let mut store_guard = store.lock().expect("hashline store poisoned");
        for file_path in seen_files {
            let path = PathBuf::from(&file_path);
            if let Ok(raw) = std::fs::read_to_string(&path) {
                let normalized = crate::hashline::normalize_to_lf(&raw);
                let _ = store_guard.record(&path, &normalized);
            }
        }
    }

    if all_results.is_empty() {
        Ok("No matches".to_string())
    } else {
        let mut out = all_results.join("\n");
        let start = offset + 1;
        let end = offset + all_results.len();
        if has_more {
            out.push_str(&format!(
                "\n[Showing matches {start}-{end}; more matches follow — narrow with path/glob/pattern, or use offset: {end} for the next page]"
            ));
        } else if offset > 0 {
            out.push_str(&format!("\n[Showing matches {start}-{end} of {seen}]"));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_dir(name: &str) -> PathBuf {
        crate::hashline::init();
        let dir =
            std::env::temp_dir().join(format!("manox-grep-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create fixture dir");
        dir
    }

    #[test]
    fn mega_line_match_is_capped() {
        // The incident shape: a single-line JSON dump must not flood the result.
        let dir = fixture_dir("mega-line");
        let blob = format!("{{\"data\":\"{}\"}}", "y".repeat(1024 * 1024));
        std::fs::write(dir.join("dump.json"), blob).expect("write fixture");
        let policy = ReadPolicy::for_project(&dir);
        let out = run_grep("data", &dir, None, None, None, &policy).expect("grep");
        assert!(out.contains("bytes truncated"), "line cap marker: {out}");
        assert!(
            out.len() < 1024,
            "mega line collapsed, got {} bytes",
            out.len()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn limit_and_offset_page_results() {
        let dir = fixture_dir("paging");
        let body: String = (1..=10).map(|i| format!("match line {i}\n")).collect();
        std::fs::write(dir.join("a.txt"), body).expect("write fixture");
        let policy = ReadPolicy::for_project(&dir);

        let page1 = run_grep("match", &dir, None, Some(3), None, &policy).expect("page1");
        assert!(page1.contains("match line 1"));
        assert!(page1.contains("match line 3"));
        assert!(!page1.contains("match line 4"));
        assert!(page1.contains("more matches follow"), "page1: {page1}");
        assert!(page1.contains("offset: 3"), "page1: {page1}");

        let page2 = run_grep("match", &dir, None, Some(3), Some(3), &policy).expect("page2");
        assert!(page2.contains("match line 4"));
        assert!(page2.contains("match line 6"));
        assert!(page2.contains("more matches follow"), "page2: {page2}");

        let last = run_grep("match", &dir, None, Some(3), Some(9), &policy).expect("last page");
        assert!(last.contains("match line 10"));
        assert!(last.contains("Showing matches 10-10 of 10"), "last: {last}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_limit_is_200() {
        let dir = fixture_dir("default-limit");
        let body: String = (1..=500).map(|i| format!("hit {i}\n")).collect();
        std::fs::write(dir.join("b.txt"), body).expect("write fixture");
        let policy = ReadPolicy::for_project(&dir);
        let out = run_grep("hit", &dir, None, None, None, &policy).expect("grep");
        let shown = out.matches("b.txt:").count();
        assert_eq!(shown, 200);
        assert!(out.contains("more matches follow"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
