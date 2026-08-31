// Read tool — reads a file, snapshots it for hashline, and returns
// `[path#TAG]` + `N:TEXT` numbered rows.
//
// An unqualified read caps at 2000 lines with a paging hint; offset/limit map
// onto a hashline `LineRange` for partial reads. Output is additionally
// truncated by a byte guard to avoid overwhelming the context window.

use serde_json::Value as JsonValue;
use tokio_util::sync::CancellationToken;

use crate::hashline::{self, LineRange};
use crate::tool::{AgentTool, AgentToolResult, ToolContext, ToolError};
use crate::tools::truncate::{self, TruncateConfig};

pub struct ReadTool;

impl ReadTool {
    /// Default max bytes for output.
    const DEFAULT_MAX_BYTES: usize = 128 * 1024;
    /// Default max lines for output.
    const DEFAULT_MAX_LINES: usize = 2000;
    /// Lines returned by an unqualified read (no offset/limit).
    const MAX_READ_LINES: usize = 2000;
}

#[async_trait::async_trait]
impl AgentTool for ReadTool {
    fn name(&self) -> &str {
        "Read"
    }
    fn description(&self) -> &str {
        "Read a file with optional line-range paging. Output format: first line \
         `[<path>#<TAG>]` (6-hex snapshot tag for follow-up edits), followed by \
         `N:TEXT` numbered rows (1-indexed). Without offset/limit the first \
         2000 lines are returned; use offset/limit to page through longer files. \
         Files over 4MB are served without the `[path#tag]` header (hashline Edit \
         is unavailable for them; use Write)."
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
                    "description": "Path to the file"
                },
                "cwd": {
                    "type": "string",
                    "description": "Working directory for this call; relative paths resolve against it. Omit to reuse the previous tool call's directory (the session's start directory initially)."
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
        _tool_call_id: &str,
        params: JsonValue,
        _signal: CancellationToken,
        ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        let path_str = params["path"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("path is required".into()))?;
        let offset = params["offset"].as_u64().map(|v| v as usize);
        let limit = params["limit"].as_u64().map(|v| v as usize);

        let cwd = crate::tools::path_utils::resolve_effective_cwd(ctx, params["cwd"].as_str())
            .map_err(ToolError::InvalidArguments)?;
        let path = cwd.join(path_str);

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

        let formatted = match (offset, limit) {
            (None, None) => format_full_read(&path_display, &text, tag),
            _ => {
                let start = offset.unwrap_or(1);
                let end = limit.map(|l| start.saturating_add(l).saturating_sub(1));
                let ranges = [LineRange { start, end }];
                hashline::format_numbered_range(&path_display, &text, tag, &ranges)
            }
        };
        let config = TruncateConfig {
            max_bytes: Self::DEFAULT_MAX_BYTES,
            max_lines: Self::DEFAULT_MAX_LINES,
        };
        let result = truncate::truncate(&formatted, &config);

        // Record the lines the OUTPUT actually shows, parsed from the possibly
        // truncated body — intent ranges would over-claim when the byte/line
        // guard clipped rows the model never received.
        let displayed = hashline::parse_seen_lines_from_body(&result.content);
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
        }
        if snap.is_none() {
            output.push_str("\n\n[no snapshot: file exceeds 4MB — hashline `Edit` is unavailable; use `Write` for changes]");
        }

        Ok(AgentToolResult::text(output))
    }
}

/// Format an unqualified read. The output caps at [`ReadTool::MAX_READ_LINES`]
/// lines — a full-file dump of a 100k-line file would flood the context; the
/// hint points the model at offset/limit paging for the rest.
fn format_full_read(path_display: &str, text: &str, tag: Option<&str>) -> String {
    const MAX: usize = ReadTool::MAX_READ_LINES;
    let line_count = text.lines().count();
    if line_count <= MAX {
        return hashline::format_numbered(path_display, text, tag);
    }
    let ranges = [LineRange {
        start: 1,
        end: Some(MAX),
    }];
    let mut out = hashline::format_numbered_range(path_display, text, tag, &ranges);
    out.push_str(&format!(
        "\n[Showing lines 1-{MAX} of {line_count}. \
         Page through the rest with offset/limit, e.g. offset {} limit {}]",
        MAX + 1,
        MAX,
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::{ToolContext, ToolState};
    use std::path::PathBuf;

    #[test]
    fn small_file_is_not_capped() {
        let text = "a\nb\nc";
        let out = format_full_read("/tmp/f.txt", text, Some("AB12"));
        assert!(out.contains("3:c"));
        assert!(!out.contains("Showing lines"));
    }

    #[test]
    fn large_file_caps_at_max_lines_with_paging_hint() {
        let text: String = (1..=5000).map(|i| format!("line {i}\n")).collect();
        let out = format_full_read("/tmp/big.txt", &text, Some("AB12"));
        assert!(out.contains("1:line 1"));
        assert!(out.contains("2000:line 2000"));
        // format_numbered_range appends 3 trailing context lines; nothing
        // beyond those may appear.
        assert!(!out.contains("2004:line 2004"));
        assert!(out.contains("Showing lines 1-2000 of 5000"));
        assert!(out.contains("offset 2001"), "paging hint: {out}");
    }

    struct Ctx {
        env: crate::env::TokioExecutionEnv,
        cwd: PathBuf,
        state: std::sync::Arc<ToolState>,
    }
    impl ToolContext for Ctx {
        fn env(&self) -> &dyn crate::env::ExecutionEnv {
            &self.env
        }
        fn cwd(&self) -> &std::path::Path {
            &self.cwd
        }
        fn tool_state(&self) -> &ToolState {
            &self.state
        }
    }

    fn ctx_at(dir: std::path::PathBuf) -> Ctx {
        Ctx {
            env: crate::env::TokioExecutionEnv::new(dir.clone()),
            cwd: dir,
            state: std::sync::Arc::new(ToolState::new()),
        }
    }

    async fn read(
        tool: &ReadTool,
        ctx: &Ctx,
        params: serde_json::Value,
    ) -> Result<AgentToolResult, crate::tool::ToolError> {
        tool.execute(
            "c1",
            params,
            tokio_util::sync::CancellationToken::new(),
            ctx,
        )
        .await
    }

    /// A relative path resolves against the call's explicit `cwd`, and the
    /// sticky advances so the next call without `cwd` inherits it.
    #[tokio::test]
    async fn explicit_cwd_resolves_relative_paths_and_advances_sticky() {
        let base = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir_in(base.path()).unwrap();
        std::fs::write(work.path().join("note.txt"), "hello worktree\n").unwrap();
        let ctx = ctx_at(base.path().to_path_buf());

        let result = read(
            &ReadTool,
            &ctx,
            serde_json::json!({
                "path": "note.txt",
                "cwd": work.path().to_string_lossy(),
            }),
        )
        .await
        .unwrap();
        match &result.content[0] {
            crate::types::ContentBlock::Text { text, .. } => {
                assert!(text.contains("hello worktree"), "{text}");
            }
            other => panic!("expected text block, got {other:?}"),
        }

        // Sticky advanced: the same relative path resolves in the worktree
        // without repeating the cwd argument.
        let inherited = read(&ReadTool, &ctx, serde_json::json!({"path": "note.txt"}))
            .await
            .unwrap();
        assert!(!inherited.is_error);
        assert_eq!(
            ctx.state.sticky_cwd.lock().unwrap().as_deref(),
            Some(work.path())
        );
    }

    /// A cwd pointing at a directory that does not exist fails before any
    /// file access and leaves the sticky untouched.
    #[tokio::test]
    async fn missing_cwd_fails_without_advancing_sticky() {
        let base = tempfile::tempdir().unwrap();
        let gone = base.path().join("gone");
        let ctx = ctx_at(base.path().to_path_buf());
        let err = read(
            &ReadTool,
            &ctx,
            serde_json::json!({"path": "any.txt", "cwd": gone.to_string_lossy()}),
        )
        .await
        .unwrap_err();
        match err {
            crate::tool::ToolError::InvalidArguments(msg) => {
                assert!(msg.contains("working directory does not exist"), "{msg}");
            }
            other => panic!("expected InvalidArguments, got {other:?}"),
        }
        assert!(ctx.state.sticky_cwd.lock().unwrap().is_none());
    }
}
