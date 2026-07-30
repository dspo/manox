// Edit diff — unified diff computation for the edit tool.
//
// When the edit tool replaces text, it also returns a unified diff showing
// the changes. This module integrates with the `similar` crate for diff
// computation.

use std::path::Path;

/// Compute a unified diff between old and new text.
///
/// Returns a unified diff string with context lines. If the texts are
/// identical, the diff is empty.
pub fn compute_unified_diff(old: &str, new: &str, path: &Path) -> String {
    let diff = similar::TextDiff::from_lines(old, new);
    diff.unified_diff()
        .context_radius(3)
        .header(&path.display().to_string(), &path.display().to_string())
        .to_string()
}

/// A structured diff hunk — a contiguous region of changes.
#[derive(Debug, Clone, PartialEq)]
pub struct DiffHunk {
    /// Starting line in the old file (1-based).
    pub old_start: usize,
    /// Number of lines in the old file affected.
    pub old_count: usize,
    /// Starting line in the new file (1-based).
    pub new_start: usize,
    /// Number of lines in the new file.
    pub new_count: usize,
    /// The lines in this hunk.
    pub lines: Vec<DiffLine>,
}

/// A single line in a diff hunk.
#[derive(Debug, Clone, PartialEq)]
pub struct DiffLine {
    pub tag: DiffLineTag,
    pub content: String,
}

/// The tag of a diff line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineTag {
    /// Unchanged context line.
    Equal,
    /// Removed line (prefixed with `-`).
    Delete,
    /// Added line (prefixed with `+`).
    Insert,
}

/// Compute structured diff hunks between old and new text.
pub fn compute_diff_hunks(old: &str, new: &str) -> Vec<DiffHunk> {
    let diff = similar::TextDiff::from_lines(old, new);
    let mut hunks = Vec::new();

    for group in diff.grouped_ops(3) {
        let mut hunk_lines = Vec::new();
        let mut old_start = 0usize;
        let mut old_count = 0usize;
        let mut new_start = 0usize;
        let mut new_count = 0usize;
        let mut first = true;

        for op in &group {
            let (os, oe, _ol, ns, ne, _nl) = match op {
                similar::DiffOp::Equal {
                    old_index,
                    new_index,
                    len,
                } => (
                    *old_index,
                    old_index + len,
                    *len,
                    *new_index,
                    new_index + len,
                    *len,
                ),
                similar::DiffOp::Delete {
                    old_index,
                    old_len,
                    new_index,
                } => (
                    *old_index,
                    old_index + old_len,
                    *old_len,
                    *new_index,
                    *new_index,
                    0,
                ),
                similar::DiffOp::Insert {
                    old_index,
                    new_index,
                    new_len,
                } => (
                    *old_index,
                    *old_index,
                    0,
                    *new_index,
                    new_index + new_len,
                    *new_len,
                ),
                similar::DiffOp::Replace {
                    old_index,
                    old_len,
                    new_index,
                    new_len,
                } => (
                    *old_index,
                    old_index + old_len,
                    *old_len,
                    *new_index,
                    new_index + new_len,
                    *new_len,
                ),
            };

            if first {
                old_start = os + 1; // 1-based
                new_start = ns + 1;
                first = false;
            }
            old_count = oe - old_start + 1;
            new_count = ne - new_start + 1;

            // Collect the changed lines for this op.
            for change in diff.iter_changes(op) {
                let tag = match change.tag() {
                    similar::ChangeTag::Equal => DiffLineTag::Equal,
                    similar::ChangeTag::Delete => DiffLineTag::Delete,
                    similar::ChangeTag::Insert => DiffLineTag::Insert,
                };
                hunk_lines.push(DiffLine {
                    tag,
                    content: change.value().to_string(),
                });
            }
        }

        hunks.push(DiffHunk {
            old_start,
            old_count,
            new_start,
            new_count,
            lines: hunk_lines,
        });
    }

    hunks
}

/// Check whether a diff is empty (no changes).
pub fn is_diff_empty(diff: &str) -> bool {
    diff.trim().is_empty()
}

/// Count the number of hunks in a unified diff.
pub fn count_diff_hunks(diff: &str) -> usize {
    diff.lines().filter(|l| l.starts_with("@@")).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_unified_diff_empty() {
        let text = "hello\nworld\n";
        let diff = compute_unified_diff(text, text, Path::new("test.txt"));
        assert!(is_diff_empty(&diff));
    }

    #[test]
    fn test_compute_unified_diff_with_changes() {
        let old = "line1\nline2\nline3\n";
        let new = "line1\nline2_modified\nline3\n";
        let diff = compute_unified_diff(old, new, Path::new("test.txt"));
        assert!(!is_diff_empty(&diff));
        assert!(diff.contains("line2_modified"));
        assert!(diff.contains("test.txt"));
    }

    #[test]
    fn test_count_diff_hunks() {
        let old = "a\nb\nc\nd\ne\nf\ng\nh\n";
        let new = "a\nb_changed\nc\nd\ne\nf_changed\ng\nh\n";
        let diff = compute_unified_diff(old, new, Path::new("test.txt"));
        let hunks = count_diff_hunks(&diff);
        assert!(hunks > 0);
    }

    #[test]
    fn test_compute_diff_hunks() {
        let old = "a\nb\nc\n";
        let new = "a\nb2\nc\n";
        let hunks = compute_diff_hunks(old, new);
        assert_eq!(hunks.len(), 1);
        let hunk = &hunks[0];
        assert!(hunk.lines.iter().any(|l| l.content.trim() == "b2"));
    }
}
