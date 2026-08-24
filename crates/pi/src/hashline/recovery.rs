//! Diff-based line mapping recovery for stale tags.
//!
//! When an edit's claimed tag no longer matches the live file (the file changed
//! between the model's read and its edit), the line numbers it references may be
//! wrong. Recovery: diff the snapshot text against the current text, build a
//! line-number map, validate every anchor's context (neighbors must map
//! consistently), remap the edit ops to current line numbers, then replay the
//! edit on the live content. All anchors must move by one consistent offset; a
//! changed, deleted, split, or ambiguous target is rejected so the caller can
//! surface an error instructing the model to re-read.
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;

use similar::TextDiff;

use super::parser::Op;
use super::snapshot::{Snapshot, SnapshotStore};

/// Recovery failure carrying a model-facing hint and the file's current tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoverError {
    pub message: String,
    pub current_tag: String,
}
impl std::fmt::Display for RecoverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RecoverError {}

#[allow(dead_code)]
/// Warning emitted when the snapshot is the current head (external change).
const RECOVERY_EXTERNAL_WARNING: &str = "file changed between read and edit (external modification); recovery applied via line-map merge";
#[allow(dead_code)]
/// Warning emitted when the snapshot is a historical version (session chain).
const RECOVERY_SESSION_CHAIN_WARNING: &str = "file changed between read and edit (edit chain); recovery applied via line-map merge";

/// Attempt to recover a stale-tagged edit. `current` is the live file text
/// (normalized LF); `claimed_tag` is the tag the edit claims. Returns the merged
/// new text on success.
pub fn try_recover(
    current: &str,
    claimed_tag: &str,
    ops: &[Op],
    store: &SnapshotStore,
    path: &Path,
) -> Result<String, RecoverError> {
    let current_tag = super::hash::compute_tag(current);
    let snapshot = store.get(path, claimed_tag).ok_or_else(|| RecoverError {
        message: format!(
            "snapshot tag {claimed_tag} not found (file changed and no history version available); re-read to get the current tag {current_tag}"
        ),
        current_tag,
    })?;
    try_recover_with_snapshot(current, snapshot, ops)
}

/// Recovery entry point with an explicit snapshot, bypassing store lookup.
pub fn try_recover_with_snapshot(
    current: &str,
    snapshot: &Snapshot,
    ops: &[Op],
) -> Result<String, RecoverError> {
    let current_tag = super::hash::compute_tag(current);
    let snapshot_text = &snapshot.text;

    // Validate the ops against the snapshot exactly as the fast path would, so
    // a stale-tag recovery never accepts a patch (overlapping ranges, insertion
    // landing inside a consumer, out-of-bounds lines, empty insertions) that the
    // fast path would reject.
    if let Err(e) = super::apply::apply(snapshot_text, ops) {
        return Err(RecoverError {
            message: e.to_string(),
            current_tag,
        });
    }

    // Build a line map from snapshot line numbers to current line numbers using
    // similar's text diff.
    let prev_lines: Vec<&str> = snapshot_text.lines().collect();
    let cur_lines: Vec<&str> = current.lines().collect();

    let line_map = build_line_map(snapshot_text, current);

    // Validate every anchor's context. For each op, collect all anchor lines
    // and check that they map consistently.
    let anchor_lines = collect_anchor_lines(ops);
    if !validate_anchors(&anchor_lines, &line_map, &prev_lines, &cur_lines) {
        return Err(RecoverError {
            message: "3-way merge failed: anchors map to inconsistent lines in the current file; re-read to get current tags and line numbers".to_string(),
            current_tag,
        });
    }

    // Remap ops to current line numbers.
    let Some(remapped) = remap_ops(ops, &line_map) else {
        return Err(RecoverError {
            message: "3-way merge failed: cannot remap anchors to current file (line numbers shifted inconsistently); re-read".to_string(),
            current_tag,
        });
    };

    // Apply remapped ops on current text.
    match super::apply::apply(current, &remapped) {
        Ok(result) => Ok(result.text),
        Err(e) => Err(RecoverError {
            message: format!("3-way merge recovery apply failed: {e}"),
            current_tag,
        }),
    }
}

/// Collect all 1-indexed anchor lines referenced by the ops.
fn collect_anchor_lines(ops: &[Op]) -> Vec<usize> {
    let mut lines = Vec::new();
    for op in ops {
        match op {
            Op::Swap { start, end, .. } => {
                for l in *start..=*end {
                    lines.push(l);
                }
            }
            Op::Del { start, end } => {
                for l in *start..=*end {
                    lines.push(l);
                }
            }
            Op::Ins { anchor: Some(a), .. } => {
                lines.push(*a);
            }
            Op::SwapBlk { start, .. } => {
                lines.push(*start);
            }
            Op::DelBlk { start } => {
                lines.push(*start);
            }
            Op::InsBlkPost { anchor, .. } => {
                lines.push(*anchor);
            }
            Op::Ins { anchor: None, .. } => {}
        }
    }
    lines
}

/// Build a 1-indexed line number map from `previous_text` to `current_text`.
/// Only unchanged lines are mapped. Returns `HashMap<prev_line, cur_line>`.
fn build_line_map(previous_text: &str, current_text: &str) -> HashMap<usize, usize> {
    let diff = TextDiff::from_lines(previous_text, current_text);
    let mut map = HashMap::new();
    let mut prev_line: usize = 1;
    let mut cur_line: usize = 1;

    for change in diff.iter_all_changes() {
        match change.tag() {
            similar::ChangeTag::Equal => {
                map.insert(prev_line, cur_line);
                prev_line += 1;
                cur_line += 1;
            }
            similar::ChangeTag::Insert => {
                cur_line += 1;
            }
            similar::ChangeTag::Delete => {
                prev_line += 1;
            }
        }
    }
    map
}

/// Values appearing two or more times in `lines`, for O(1) duplicate checks.
fn collect_duplicates<'a>(lines: &'a [&'a str]) -> HashSet<&'a str> {
    let mut seen = HashSet::new();
    let mut dup = HashSet::new();
    for &v in lines {
        if !seen.insert(v) {
            dup.insert(v);
        }
    }
    dup
}

/// Nearest non-anchor context line on each side of an anchor, when the anchor
/// sits in a contiguous run of anchors.
struct AnchorNeighbors {
    before: Option<usize>, // 1-indexed line just above the anchor run
    after: Option<usize>,  // 1-indexed line just below the anchor run
}

/// Compute nearest non-anchor context for every anchor in one pass.
fn compute_anchor_neighbors(
    anchor_lines: &BTreeSet<usize>,
    line_count: usize,
) -> HashMap<usize, AnchorNeighbors> {
    let sorted: Vec<&usize> = anchor_lines.iter().collect();
    let mut neighbors = HashMap::new();
    let mut i = 0;
    while i < sorted.len() {
        let mut j = i;
        while j + 1 < sorted.len() && sorted[j + 1] == &(sorted[j] + 1) {
            j += 1;
        }
        let start = *sorted[i];
        let end = *sorted[j];
        let before = if start > 1 && start <= line_count { Some(start - 1) } else { None };
        let after = if end < line_count { Some(end + 1) } else { None };
        for &anchor in &sorted[i..=j] {
            neighbors.insert(*anchor, AnchorNeighbors { before, after });
        }
        i = j + 1;
    }
    neighbors
}

/// Validate that every anchor's context also maps consistently.
fn validate_anchors(
    anchors: &[usize],
    line_map: &HashMap<usize, usize>,
    prev_lines: &[&str],
    cur_lines: &[&str],
) -> bool {
    if anchors.is_empty() {
        return true;
    }

    let anchor_set: BTreeSet<usize> = anchors.iter().copied().collect();
    let duplicated_prev = collect_duplicates(prev_lines);
    let duplicated_cur = collect_duplicates(cur_lines);
    let neighbors = compute_anchor_neighbors(&anchor_set, prev_lines.len());

    for &line in &anchor_set {
        let Some(&mapped) = line_map.get(&line) else {
            return false;
        };
        if mapped == 0 || mapped > cur_lines.len() {
            return false;
        }
        let Some(n) = neighbors.get(&line) else {
            // Single anchor with no context at file boundary — accept if unique.
            if prev_lines[line - 1].is_empty() || !duplicated_prev.contains(prev_lines[line - 1]) {
                continue;
            }
            return false;
        };

        let prev_is_dup = duplicated_prev.contains(prev_lines[line - 1]);
        let cur_is_dup = duplicated_cur.contains(cur_lines[mapped - 1]);

        if !prev_is_dup && !cur_is_dup {
            // Unique value: at least one neighbor must map consistently.
            let mut ok = false;
            if let Some(before) = n.before
                && line_map.get(&before) == Some(&(mapped.saturating_sub(line - before)))
            {
                ok = true;
            }
            if !ok
                && let Some(after) = n.after
                && line_map.get(&after) == Some(&(mapped + (after - line)))
            {
                ok = true;
            }
            if !ok {
                return false;
            }
        } else {
            // Duplicate value: BOTH neighbors must map consistently.
            if let Some(before) = n.before
                && line_map.get(&before) != Some(&(mapped.saturating_sub(line - before)))
            {
                return false;
            }
            if let Some(after) = n.after
                && line_map.get(&after) != Some(&(mapped + (after - line)))
            {
                return false;
            }
        }
    }
    true
}

/// Remap ops from snapshot line numbers to current line numbers.
/// Returns `None` when any anchor is unmapped or the offsets are inconsistent.
fn remap_ops(ops: &[Op], line_map: &HashMap<usize, usize>) -> Option<Vec<Op>> {
    let mut offsets: Vec<isize> = Vec::new();

    let mut map_line = |line: usize| -> Option<usize> {
        let mapped = *line_map.get(&line)?;
        let offset = mapped as isize - line as isize;
        offsets.push(offset);
        Some(mapped)
    };

    let mut remapped = Vec::with_capacity(ops.len());
    for op in ops {
        match op {
            Op::Swap { start, end, body } => {
                let s = map_line(*start)?;
                let e = map_line(*end)?;
                remapped.push(Op::Swap {
                    start: s,
                    end: e,
                    body: body.clone(),
                });
            }
            Op::Del { start, end } => {
                let s = map_line(*start)?;
                let e = map_line(*end)?;
                remapped.push(Op::Del { start: s, end: e });
            }
            Op::Ins { pos, anchor, body } => {
                let new_anchor = anchor.and_then(&mut map_line);
                remapped.push(Op::Ins {
                    pos: *pos,
                    anchor: new_anchor,
                    body: body.clone(),
                });
            }
            Op::SwapBlk { start, body } => {
                let s = map_line(*start)?;
                remapped.push(Op::SwapBlk {
                    start: s,
                    body: body.clone(),
                });
            }
            Op::DelBlk { start } => {
                let s = map_line(*start)?;
                remapped.push(Op::DelBlk { start: s });
            }
            Op::InsBlkPost { anchor, body } => {
                let a = map_line(*anchor)?;
                remapped.push(Op::InsBlkPost {
                    anchor: a,
                    body: body.clone(),
                });
            }
        }
    }

    // All anchors must have the same offset.
    // HEAD/TAIL inserts have no anchors and produce an empty offsets vec.
    if offsets.is_empty() {
        // No anchors to remap — return ops unchanged. This is valid for
        // HEAD/TAIL-only patches. But if there ARE anchor-bearing ops, we
        // should have offsets.
        let has_anchors = ops.iter().any(|op| {
            matches!(
                op,
                Op::Swap { .. }
                    | Op::Del { .. }
                    | Op::SwapBlk { .. }
                    | Op::DelBlk { .. }
                    | Op::InsBlkPost { .. }
                    | Op::Ins {
                        anchor: Some(_),
                        ..
                    }
            )
        });
        if has_anchors {
            return None;
        }
        return Some(remapped);
    }
    let first = offsets[0];
    if offsets.iter().all(|&o| o == first) {
        Some(remapped)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use crate::hashline::InsPos;

    fn snap(tag: &str, text: &str) -> Snapshot {
        Snapshot {
            path: PathBuf::from("x.rs"),
            text: text.to_string(),
            tag: tag.to_string(),
            recorded_at: 0,
            seen_lines: None,
        }
    }

    #[test]
    fn missing_snapshot_errors() {
        let store = SnapshotStore::new();
        let err = try_recover("A\nB\n", "FFFFFF", &[], &store, &PathBuf::from("x.rs")).unwrap_err();
        assert!(err.message.contains("not found"));
    }

    #[test]
    fn current_equals_snapshot_applies_cleanly() {
        let text = "fn a() {\n    x();\n}\n";
        let snapshot = snap("AAAAAA", text);
        let ops = [Op::Swap {
            start: 2,
            end: 2,
            body: vec!["    y();".into()],
        }];
        let merged = try_recover_with_snapshot(text, &snapshot, &ops).unwrap();
        assert_eq!(merged, "fn a() {\n    y();\n}");
    }

    #[test]
    fn shifted_context_still_locates_target() {
        let snap_text = "fn a() {\n    x();\n}\n";
        let current = "// header\nfn a() {\n    x();\n}\n";
        let snapshot = snap("AAAAAA", snap_text);
        let ops = [Op::Swap {
            start: 2,
            end: 2,
            body: vec!["    y();".into()],
        }];
        let merged = try_recover_with_snapshot(current, &snapshot, &ops).unwrap();
        assert_eq!(merged, "// header\nfn a() {\n    y();\n}");
    }

    #[test]
    fn shifted_context_tail_insertion() {
        let snap_text = "a\nb\nc";
        let current = "a\nb\nc\nd";
        let snapshot = snap("BBBBBB", snap_text);
        let ops = [Op::Ins {
            pos: InsPos::Tail,
            anchor: None,
            body: vec!["e".into()],
        }];
        let merged = try_recover_with_snapshot(current, &snapshot, &ops).unwrap();
        assert_eq!(merged, "a\nb\nc\nd\ne");
    }

    #[test]
    fn ambiguous_anchor_succeeds_for_first_occurrence() {
        let snap_text = "fn a() {\n    x();\n}\n";
        let current = "fn a() {\n    x();\n}\nfn b() {\n    x();\n}\n";
        let snapshot = snap("AAAAAA", snap_text);
        let ops = [Op::Swap {
            start: 2,
            end: 2,
            body: vec!["    y();".into()],
        }];
        // The diff maps line 2 → 2 (first `x();`). The second `x();` is at
        // line 6 in current, not in the line map. So this should succeed.
        let merged = try_recover_with_snapshot(current, &snapshot, &ops).unwrap();
        assert_eq!(merged, "fn a() {\n    y();\n}\nfn b() {\n    x();\n}");
    }

    #[test]
    fn missing_target_fails() {
        let snap_text = "fn a() {\n    x();\n}\n";
        let current = "fn a() {\n    z();\n}\n";
        let snapshot = snap("AAAAAA", snap_text);
        let ops = [Op::Del { start: 2, end: 2 }];
        assert!(try_recover_with_snapshot(current, &snapshot, &ops).is_err());
    }

    #[test]
    fn multiple_hunks_shifted() {
        let snap_text = "a\nb\nc\nd\ne";
        let current = "HEAD\na\nb\nc\nd\ne";
        let snapshot = snap("CCCCCC", snap_text);
        let ops = [
            Op::Swap {
                start: 1,
                end: 1,
                body: vec!["x".into()],
            },
            Op::Swap {
                start: 5,
                end: 5,
                body: vec!["y".into()],
            },
        ];
        let merged = try_recover_with_snapshot(current, &snapshot, &ops).unwrap();
        assert_eq!(merged, "HEAD\nx\nb\nc\nd\ny");
    }

    #[test]
    fn insert_after_shifted_anchor() {
        let snap_text = "a\nb\nc";
        let current = "x\na\nb\nc";
        let snapshot = snap("DDDDDD", snap_text);
        let ops = [Op::Ins {
            pos: InsPos::Post,
            anchor: Some(2),
            body: vec!["INS".into()],
        }];
        let merged = try_recover_with_snapshot(current, &snapshot, &ops).unwrap();
        assert_eq!(merged, "x\na\nb\nINS\nc");
    }
}