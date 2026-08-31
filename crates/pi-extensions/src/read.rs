//! Read tool with oh-my-pi style `path:selector` syntax.
//!
//! Wraps the kernel [`pi::tools::read::ReadTool`]: a `path` without a
//! selector delegates unchanged (offset/limit paging preserved); a
//! `path:selector` suffix routes through the selector grammar
//! ([`crate::path_selector`]) — numbered ranges, `:raw` verbatim, or a
//! compound of both. TS Pi's Read has no selector syntax; this is the
//! product-level extension riding the `AgentTool` seam, so the kernel tool
//! stays TS-aligned.
//!
//! Selector reads keep the kernel's invariants: the hashline snapshot
//! fingerprints the FULL file (only display is sliced), and the same byte /
//! line truncation guard applies.

use pi::hashline::{self};
use pi::tool::{AgentTool, AgentToolResult, ToolContext, ToolError};
use pi::tools::read::ReadTool;
use pi::tools::truncate::{self, TruncateConfig};
use serde_json::Value as JsonValue;
use tokio_util::sync::CancellationToken;

use crate::path_selector::{Selector, split_path_and_sel};

/// `Read` with `path:selector` support. Delegates selector-less reads to the
/// kernel [`ReadTool`].
pub struct SelectorReadTool {
    inner: ReadTool,
}

impl SelectorReadTool {
    /// Output guards mirror the kernel Read: byte cap first, line cap second.
    const DEFAULT_MAX_BYTES: usize = 128 * 1024;
    const DEFAULT_MAX_LINES: usize = 2000;

    pub fn new() -> Self {
        Self { inner: ReadTool }
    }
}

impl Default for SelectorReadTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl AgentTool for SelectorReadTool {
    fn name(&self) -> &str {
        "Read"
    }

    fn description(&self) -> &str {
        "Read a file with optional line-range paging or a path selector. Output \
         format: first line `[<path>#<TAG>]` (6-hex snapshot tag for follow-up \
         edits), followed by `N:TEXT` numbered rows (1-indexed). Without \
         offset/limit the first 2000 lines are returned; use offset/limit to \
         page through longer files. The path may carry a selector after the \
         last colon: `:N` / `:N-` (from line N), `:N-M` inclusive range, \
         `:N+K` (K lines from N), `:N..M` alias, `:5-16,960-973` multi-range \
         (sorted, merged), `:raw` verbatim without header/line numbers, or a \
         compound `:raw:1-50` / `:1-50:raw`. A selector overrides \
         offset/limit."
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn parameters_schema(&self) -> JsonValue {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file, optionally with a line selector after the last colon (e.g. `src/a.rs:50-100`, `src/a.rs:5-16,960-973`, `src/a.rs:raw`)"
                },
                "offset": {
                    "type": "integer",
                    "description": "Line number to start reading from (1-based)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of lines to read"
                }
            },
            "required": ["path"]
        })
    }

    async fn execute(
        &self,
        tool_call_id: &str,
        params: JsonValue,
        signal: CancellationToken,
        ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        let path_str = params["path"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("path is required".into()))?;
        let (base, selector) = split_path_and_sel(path_str);
        let Some(selector) = selector else {
            // No selector: the kernel tool owns the read (offset/limit paging,
            // 2000-line unqualified cap, truncation guard).
            return self.inner.execute(tool_call_id, params, signal, ctx).await;
        };
        // Selector reads ignore offset/limit — the selector is the explicit
        // range statement (old manox semantics).
        let path = ctx.cwd().join(base);
        let raw = ctx
            .env()
            .read_file(&path, None, None)
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("{e}")))?;
        let text = hashline::normalize_to_lf(&raw);
        let path_display = path.display().to_string();

        // The snapshot always fingerprints the full file — only display is
        // sliced. Files over the snapshot cap carry no tag and no header.
        let snap = {
            let mut store = ctx
                .tool_state()
                .snapshots
                .lock()
                .expect("hashline snapshot store poisoned");
            hashline::record_read_snapshot(&mut store, &path, &text)
        };
        let tag = snap.as_ref().map(|s| s.tag.as_str());

        let raw_selector = matches!(selector, Selector::Raw | Selector::RawLines(_));
        let formatted = match &selector {
            Selector::Lines(ranges) => {
                hashline::format_numbered_range(&path_display, &text, tag, ranges)
            }
            Selector::Raw => hashline::format_raw(&text, None),
            Selector::RawLines(ranges) => hashline::format_raw(&text, Some(ranges)),
        };
        let config = TruncateConfig {
            max_bytes: Self::DEFAULT_MAX_BYTES,
            max_lines: Self::DEFAULT_MAX_LINES,
        };
        // Head-contiguous truncation: numbered rows must be trusted as fully
        // shown in order. A head+tail hole would silently un-see the middle
        // of the range while the model believes it read everything.
        let result = truncate::truncate_head(&formatted, &config);

        // Record the lines the OUTPUT actually shows. Numbered bodies carry
        // their line numbers, so the (possibly truncated) body parses
        // directly; raw bodies carry no numbers, so the requested ranges are
        // trusted only when the output was not clipped. Over-claiming would
        // let the gate accept edits on lines the model never received.
        let displayed: std::collections::HashSet<usize> = if raw_selector {
            if result.was_truncated {
                std::collections::HashSet::new()
            } else {
                match &selector {
                    Selector::Raw => (1..=text.lines().count()).collect(),
                    Selector::RawLines(ranges) => {
                        let total = text.lines().count();
                        ranges
                            .iter()
                            .flat_map(|r| {
                                let start = r.start;
                                let end = r.end.unwrap_or(total).min(total);
                                start..=end
                            })
                            .collect()
                    }
                    Selector::Lines(_) => std::collections::HashSet::new(),
                }
            }
        } else {
            hashline::parse_seen_lines_from_body(&result.content)
        };
        if !displayed.is_empty()
            && let Some(snap) = snap.as_ref()
        {
            ctx.tool_state()
                .snapshots
                .lock()
                .expect("hashline snapshot store poisoned")
                .record_seen_lines(&path, &snap.tag, &displayed);
        }
        let mut output = result.content;
        if result.was_truncated {
            output.push_str(&format!(
                "\n\n[read: {} lines, {} bytes — output truncated]",
                result.original_lines, result.original_bytes
            ));
            // Selector reads don't get the kernel's offset/limit paging hint;
            // point the model at the selector continuation instead, naming
            // the concrete next row for numbered bodies so paging neither
            // re-reads nor skips rows.
            let next = if raw_selector {
                None
            } else {
                displayed.iter().copied().max().map(|m| m + 1)
            };
            let cont = match next {
                Some(next) => format!(
                    "continue from line {next}, e.g. `{path_display}:{next}-{}`",
                    next + 199
                ),
                None => {
                    "continue with a bounded raw selector, e.g. `<path>:raw:FROM-TO`".to_string()
                }
            };
            output.push_str(&format!("\n[{cont}]"));
        }
        if snap.is_none() {
            output.push_str("\n\n[no snapshot: file exceeds 4MB — hashline `Edit` is unavailable; use `Write` for changes]");
        }
        Ok(AgentToolResult::text(output))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi::env::TokioExecutionEnv;
    use pi::tool::LocalToolContext;
    use std::sync::Arc;

    fn ctx(dir: &std::path::Path) -> LocalToolContext {
        LocalToolContext::new(
            Arc::new(TokioExecutionEnv::new(dir)),
            dir.to_path_buf(),
            Arc::new(pi::tool::ToolState::new()),
        )
    }

    fn params(path: &str) -> JsonValue {
        serde_json::json!({ "path": path })
    }

    async fn read(dir: &std::path::Path, path: &str) -> String {
        let tool = SelectorReadTool::new();
        let ctx = ctx(dir);
        let result = tool
            .execute("t1", params(path), CancellationToken::new(), &ctx)
            .await
            .expect("read should succeed");
        match &result.content[0] {
            pi::types::ContentBlock::Text { text, .. } => text.clone(),
            other => panic!("unexpected content: {other:?}"),
        }
    }

    #[tokio::test]
    async fn selectorless_read_delegates_to_kernel() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let out = read(dir.path(), "a.txt").await;
        // Kernel Read formatting: `[path#TAG]` header + numbered rows.
        assert!(out.starts_with("[a.txt#") || out.contains("#"), "{out}");
        assert!(out.contains("1:one"), "{out}");
        assert!(out.contains("3:three"), "{out}");
    }

    /// 20 numbered lines (`line1`..`line20`) — long enough that the kernel's
    /// context expansion (1 leading + 3 trailing) doesn't swallow the file.
    fn twenty_lines() -> String {
        (1..=20).map(|i| format!("line{i}\n")).collect()
    }

    #[tokio::test]
    async fn range_selector_slices_numbered_output() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), twenty_lines()).unwrap();
        let out = read(dir.path(), "a.txt:8-10").await;
        assert!(out.contains("8:line8"), "{out}");
        assert!(out.contains("10:line10"), "{out}");
        // Context window is 1 leading + 3 trailing: lines outside stay out.
        assert!(!out.contains("6:line6"), "{out}");
        assert!(!out.contains("14:line14"), "{out}");
    }

    #[tokio::test]
    async fn multi_range_selector_keeps_both_segments() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), twenty_lines()).unwrap();
        let out = read(dir.path(), "a.txt:3-4,15-16").await;
        assert!(out.contains("3:line3"), "{out}");
        assert!(out.contains("4:line4"), "{out}");
        assert!(out.contains("15:line15"), "{out}");
        assert!(out.contains("16:line16"), "{out}");
        // The gap between the two context windows renders as an ellipsis.
        assert!(out.contains("..."), "{out}");
        assert!(!out.contains("9:line9"), "{out}");
    }

    #[tokio::test]
    async fn raw_selector_drops_header_and_numbers() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        let out = read(dir.path(), "a.txt:raw").await;
        assert!(!out.contains("1:"), "{out}");
        assert!(!out.starts_with('['), "{out}");
        assert_eq!(out.trim_end_matches('\n'), "one\ntwo");
    }

    #[tokio::test]
    async fn raw_range_selector_combines() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "a\nb\nc\n").unwrap();
        let out = read(dir.path(), "a.txt:raw:2-3").await;
        assert_eq!(out.trim_end_matches('\n'), "b\nc");
    }

    #[tokio::test]
    async fn invalid_selector_falls_back_to_path() {
        let dir = tempfile::tempdir().unwrap();
        // `:xyz` is not selector grammar — the whole string stays the path,
        // and the read fails like any missing file (colon included).
        std::fs::write(dir.path().join("a.txt"), "x\n").unwrap();
        let tool = SelectorReadTool::new();
        let ctx = ctx(dir.path());
        let err = tool
            .execute(
                "t1",
                serde_json::json!({ "path": "a.txt:xyz" }),
                CancellationToken::new(),
                &ctx,
            )
            .await
            .expect_err("colon-bearing non-selector path should miss the file");
        assert!(matches!(err, ToolError::ExecutionFailed(_)), "{err:?}");
    }
}
