//! 3-way merge recovery for stale tags.
//!
//! When an edit's claimed tag no longer matches the live file (the file changed
//! between the model's read and its edit), the line numbers it references may be
//! wrong. Recovery: take the snapshot text the tag *did* name, resolve each op
//! against that snapshot to get `(old_lines, new_body)` pairs, then locate
//! `old_lines` by *content* in the current file and apply the same change there.
//! This salvages edits whose target region is unchanged even when nearby lines
//! have shifted.
//!
//! If the snapshot is missing (the claimed tag was never recorded) or an anchor
//! cannot be located uniquely, the caller surfaces an error instructing the
//! model to re-read.

use std::path::Path;

use super::apply::repair_boundaries;
use super::block;
use super::parser::{InsPos, Op};
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
            "快照 tag {claimed_tag} 未找到（文件已变更且无历史版本）；请重新 read 获取当前 tag {current_tag}"
        ),
        current_tag,
    })?;
    try_recover_with_snapshot(current, snapshot, ops)
}

/// Recovery entry point with an explicit snapshot (used by tests).
pub fn try_recover_with_snapshot(
    current: &str,
    snapshot: &Snapshot,
    ops: &[Op],
) -> Result<String, RecoverError> {
    let current_tag = super::hash::compute_tag(current);
    let snap_lines: Vec<&str> = snapshot.text.lines().collect();
    let cur_lines: Vec<&str> = current.lines().collect();

    // Resolve each op to a content-anchored change against the snapshot, then
    // project onto current. Block ops resolve to concrete snapshot ranges first.
    let changes = resolve_changes(ops, &snap_lines, &current_tag)?;

    // Apply changes back-to-front by their located position in `current` so
    // earlier anchors stay valid. Locate each anchor fresh (positions are
    // content-derived, not line-derived, so ordering by snapshot line is not
    // meaningful — collect located positions first, then sort descending).
    let mut edits: Vec<LocatedEdit> = Vec::new();
    for ch in changes {
        let loc = locate(&cur_lines, &ch).ok_or_else(|| RecoverError {
            message: format!(
                "3-way merge 失败：在当前文件中无法唯一锚定快照行「{}」；请重新 read",
                ch.anchor_preview()
            ),
            current_tag: current_tag.clone(),
        })?;
        edits.push(loc);
    }

    // Sort descending by insertion index (back-to-front).
    edits.sort_by_key(|e| std::cmp::Reverse(e.at));
    let mut out: Vec<String> = cur_lines.iter().map(|s| s.to_string()).collect();
    for e in edits {
        e.apply(&mut out);
    }
    Ok(out.join("\n"))
}

/// A change resolved against the snapshot: the old line slice to find in
/// current, and the new body to substitute (empty body = pure deletion).
enum Change {
    /// Replace `old` (non-empty) with `body` at the unique location of `old`.
    Replace { old: Vec<String>, body: Vec<String> },
    /// Insert `body` before/after the unique location of `anchor` (a single line).
    Insert {
        anchor: Vec<String>,
        body: Vec<String>,
        before: bool,
    },
    /// Insert `body` at file start or end.
    InsertEnd { body: Vec<String>, head: bool },
}

impl Change {
    fn anchor_preview(&self) -> String {
        match self {
            Change::Replace { old, .. } => old.first().cloned().unwrap_or_default(),
            Change::Insert { anchor, .. } => anchor.first().cloned().unwrap_or_default(),
            Change::InsertEnd { .. } => "<file boundary>".to_string(),
        }
    }
}

struct LocatedEdit {
    at: usize,  // 0-indexed insertion/replacement start in current
    end: usize, // exclusive replacement end (== at for insertions)
    body: Vec<String>,
}

impl LocatedEdit {
    fn apply(&self, out: &mut Vec<String>) {
        out.splice(self.at..self.end, self.body.iter().cloned());
    }
}

fn resolve_changes(
    ops: &[Op],
    snap_lines: &[&str],
    current_tag: &str,
) -> Result<Vec<Change>, RecoverError> {
    let mut changes = Vec::new();
    for op in ops {
        match op {
            Op::Swap { start, end, body } => {
                let s = start.checked_sub(1).unwrap_or(0);
                let e = *end; // exclusive
                let old: Vec<String> = snap_lines
                    .get(s..e)
                    .ok_or_else(|| RecoverError {
                        message: format!("快照范围 {start}..={end} 越界"),
                        current_tag: current_tag.to_string(),
                    })?
                    .iter()
                    .map(|l| l.to_string())
                    .collect();
                let repaired = repair_boundaries(snap_lines, *start, *end, body);
                changes.push(Change::Replace {
                    old,
                    body: repaired,
                });
            }
            Op::Del { start, end } => {
                let s = start.checked_sub(1).unwrap_or(0);
                let e = *end;
                let old: Vec<String> = snap_lines
                    .get(s..e)
                    .ok_or_else(|| RecoverError {
                        message: format!("快照范围 {start}..={end} 越界"),
                        current_tag: current_tag.to_string(),
                    })?
                    .iter()
                    .map(|l| l.to_string())
                    .collect();
                changes.push(Change::Replace {
                    old,
                    body: Vec::new(),
                });
            }
            Op::Ins { pos, anchor, body } => {
                let (anchor, before, head, tail) = match (pos, anchor) {
                    (InsPos::Head, _) => (None, false, true, false),
                    (InsPos::Tail, _) => (None, false, false, true),
                    (InsPos::Pre, Some(a)) => (Some(*a), true, false, false),
                    (InsPos::Post, Some(a)) => (Some(*a), false, false, false),
                    _ => {
                        return Err(RecoverError {
                            message: "INS PRE/POST 缺少锚点".to_string(),
                            current_tag: current_tag.to_string(),
                        });
                    }
                };
                if head || tail {
                    changes.push(Change::InsertEnd {
                        body: body.clone(),
                        head,
                    });
                    continue;
                }
                let a = anchor.unwrap().saturating_sub(1);
                let anchor_line =
                    snap_lines
                        .get(a)
                        .map(|l| l.to_string())
                        .ok_or_else(|| RecoverError {
                            message: format!("INS 锚点行 {} 越界", a + 1),
                            current_tag: current_tag.to_string(),
                        })?;
                changes.push(Change::Insert {
                    anchor: vec![anchor_line],
                    body: body.clone(),
                    before,
                });
            }
            Op::SwapBlk { start, body } => {
                let (s, e) =
                    block::resolve_block_range(snap_lines, *start).map_err(|e| RecoverError {
                        message: e.to_string(),
                        current_tag: current_tag.to_string(),
                    })?;
                let old: Vec<String> = snap_lines[s - 1..e].iter().map(|l| l.to_string()).collect();
                let repaired = repair_boundaries(snap_lines, s, e, body);
                changes.push(Change::Replace {
                    old,
                    body: repaired,
                });
            }
            Op::DelBlk { start } => {
                let (s, e) =
                    block::resolve_block_range(snap_lines, *start).map_err(|e| RecoverError {
                        message: e.to_string(),
                        current_tag: current_tag.to_string(),
                    })?;
                let old: Vec<String> = snap_lines[s - 1..e].iter().map(|l| l.to_string()).collect();
                changes.push(Change::Replace {
                    old,
                    body: Vec::new(),
                });
            }
            Op::InsBlkPost { anchor, body } => {
                let land = block::block_post_insertion_line(snap_lines, *anchor).map_err(|e| {
                    RecoverError {
                        message: e.to_string(),
                        current_tag: current_tag.to_string(),
                    }
                })?;
                // Anchor on the line at `land` (1-indexed); insert after it.
                let a = land.saturating_sub(1);
                let anchor_line = snap_lines.get(a).map(|l| l.to_string()).unwrap_or_default();
                changes.push(Change::Insert {
                    anchor: vec![anchor_line],
                    body: body.clone(),
                    before: false,
                });
            }
        }
    }
    Ok(changes)
}

/// Locate `old` (a non-empty line sequence) in `cur_lines` uniquely; return the
/// 0-indexed `(at, end)` range. For insertions, locate the single `anchor` line
/// and return `(at, at)` with `at` set to the insertion point.
fn locate(cur_lines: &[&str], change: &Change) -> Option<LocatedEdit> {
    match change {
        Change::Replace { old, body } => {
            if old.is_empty() {
                return None;
            }
            let pos = find_unique_run(cur_lines, old)?;
            Some(LocatedEdit {
                at: pos,
                end: pos + old.len(),
                body: body.clone(),
            })
        }
        Change::Insert {
            anchor,
            body,
            before,
        } => {
            let pos = find_unique_run(cur_lines, anchor)?;
            let at = if *before { pos } else { pos + anchor.len() };
            Some(LocatedEdit {
                at,
                end: at,
                body: body.clone(),
            })
        }
        Change::InsertEnd { body, head } => {
            let at = if *head { 0 } else { cur_lines.len() };
            Some(LocatedEdit {
                at,
                end: at,
                body: body.clone(),
            })
        }
    }
}

/// Find the first (and require unique) index where `needle` appears as a
/// contiguous sub-sequence of `haystack`. Returns `None` if not found or
/// ambiguous (more than one match).
fn find_unique_run(haystack: &[&str], needle: &[String]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    let mut first: Option<usize> = None;
    let mut count = 0;
    for i in 0..=(haystack.len() - needle.len()) {
        if haystack[i..i + needle.len()]
            .iter()
            .zip(needle.iter())
            .all(|(h, n)| *h == n.as_str())
        {
            first = Some(i);
            count += 1;
            if count > 1 {
                return None;
            }
        }
    }
    first.filter(|_| count == 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn snap(tag: &str, text: &str) -> Snapshot {
        Snapshot {
            path: PathBuf::from("x.rs"),
            text: text.to_string(),
            tag: tag.to_string(),
        }
    }

    #[test]
    fn missing_snapshot_errors() {
        let store = SnapshotStore::new();
        let err = try_recover("A\nB\n", "FFFF", &[], &store, &PathBuf::from("x.rs")).unwrap_err();
        assert!(err.message.contains("未找到"));
    }

    #[test]
    fn current_equals_snapshot_applies_cleanly() {
        let text = "fn a() {\n    x();\n}\n";
        let snapshot = snap("AAAA", text);
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
        // Snapshot: target line `x();` is at line 2. Current prepended an
        // unrelated header line, shifting `x();` to line 3 — but content
        // anchoring still finds it.
        let snap_text = "fn a() {\n    x();\n}\n";
        let current = "// header\nfn a() {\n    x();\n}\n";
        let snapshot = snap("AAAA", snap_text);
        let ops = [Op::Swap {
            start: 2,
            end: 2,
            body: vec!["    y();".into()],
        }];
        let merged = try_recover_with_snapshot(current, &snapshot, &ops).unwrap();
        assert_eq!(merged, "// header\nfn a() {\n    y();\n}");
    }

    #[test]
    fn ambiguous_anchor_fails() {
        // `    x();` (snapshot's line 2) appears twice in current → cannot
        // uniquely locate the swap target.
        let snap_text = "fn a() {\n    x();\n}\n";
        let current = "fn a() {\n    x();\n}\nfn b() {\n    x();\n}\n";
        let snapshot = snap("AAAA", snap_text);
        let ops = [Op::Swap {
            start: 2,
            end: 2,
            body: vec!["    y();".into()],
        }];
        assert!(try_recover_with_snapshot(current, &snapshot, &ops).is_err());
    }

    #[test]
    fn missing_target_fails() {
        let snap_text = "fn a() {\n    x();\n}\n";
        let current = "fn a() {\n    z();\n}\n";
        let snapshot = snap("AAAA", snap_text);
        let ops = [Op::Del { start: 2, end: 2 }];
        assert!(try_recover_with_snapshot(current, &snapshot, &ops).is_err());
    }
}
