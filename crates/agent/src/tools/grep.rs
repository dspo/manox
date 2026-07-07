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

use crate::tool::AgentTool;

use super::{resolve_path, schema};

pub struct GrepTool {
    pub(crate) cwd: Arc<PathBuf>,
}

#[derive(Deserialize, JsonSchema)]
struct GrepInput {
    /// Regex pattern.
    pattern: String,
    /// Search root directory (defaults to cwd).
    #[serde(default)]
    path: Option<String>,
    /// Optional filename glob filter (e.g. `*.rs`).
    #[serde(default)]
    glob: Option<String>,
}

impl AgentTool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }
    fn description(&self) -> &str {
        "Search file contents by regex, returning matching lines (with line numbers)."
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
        let Ok(parsed) = serde_json::from_value::<GrepInput>(input) else {
            return cx.background_spawn(async { Err("input parse failed".to_string()) });
        };
        let base = parsed
            .path
            .map(|p| resolve_path(&p, &self.cwd))
            .unwrap_or_else(|| self.cwd.as_ref().clone());
        cx.background_spawn(async move { run_grep(&parsed.pattern, &base, parsed.glob.as_deref()) })
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
        let content = String::from_utf8_lossy(mat.bytes()).trim_end().to_string();
        self.results
            .push(format!("{}:{}:{}", self.path, line, content));
        Ok(true)
    }
}

fn run_grep(pattern: &str, root: &Path, glob: Option<&str>) -> Result<String, String> {
    let matcher = RegexMatcherBuilder::new()
        .build(pattern)
        .map_err(|e| format!("grep invalid regex: {e}"))?;

    let mut walker = WalkBuilder::new(root);
    walker.standard_filters(true);
    walker.hidden(false);

    if let Some(g) = glob {
        let overrides = OverrideBuilder::new(root)
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
    for result in walker.build() {
        let entry = match result {
            Ok(e) => e,
            Err(_) => continue,
        };
        let is_file = entry.file_type().map(|ft| ft.is_file()).unwrap_or(false);
        if !is_file {
            continue;
        }
        let file_path = entry.path();
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
        all_results.append(&mut sink.results);
    }

    if all_results.is_empty() {
        Ok("No matches".to_string())
    } else {
        Ok(all_results.join("\n"))
    }
}
