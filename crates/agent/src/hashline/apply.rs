//! Apply parsed operations to file text.
//!
//! Operations reference the ORIGINAL file's line numbers and do not shift as
//! hunks apply. To preserve this, ops are applied back-to-front (highest line
//! first) so a later op's line numbers are untouched by earlier edits. Two ops
//! that target overlapping or identical ranges are rejected.
//!
//! Boundary repair (subset of oh-my-pi's `apply.ts`): a `SWAP` body that
//! restates lines immediately above or below the range — the common model
//! mistake of echoing context — is trimmed of those duplicated rows so the
//! repair never silently shreds a keeper line. A suffix echo is dropped only
//! when the echoed row is a closer or neutral; a prefix echo only when it is an
//! opener or neutral. Ambiguous cases fall through unchanged and let the
//! model's payload stand.

use super::block;
use super::parser::{InsPos, Op};

/// Apply failure with a model-facing message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyError {
    pub message: String,
}

impl std::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ApplyError {}

/// The result of applying ops: the new text and the first changed 1-indexed line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyResult {
    pub text: String,
    pub first_changed_line: usize,
}

/// Apply `ops` to `text`. `text` is the normalized (LF) file content.
pub fn apply(text: &str, ops: &[Op]) -> Result<ApplyResult, ApplyError> {
    let mut lines: Vec<String> = text.lines().map(|s| s.to_string()).collect();
    let original_len = lines.len();

    // Resolve block ops to concrete line ranges first, then sort all ops
    // back-to-front by their primary line so original line numbers stay valid.
    let mut resolved: Vec<ResolvedOp> = Vec::with_capacity(ops.len());
    for op in ops {
        resolved.push(resolve_op(op, &lines)?);
    }

    // Detect range conflicts before mutating. Two kinds conflict:
    //   1. Two consuming ops (Swap/Del) whose `[start, end]` ranges overlap.
    //   2. An insertion whose landing index falls inside a consuming op's range
    //      — back-to-front application would let the consumer eat the inserted
    //      rows (e.g. `INS.PRE 5` + `SWAP 5.=5` loses the inserted rows).
    // Pure insertions may share an anchor (they stack by stable order). The
    // HEAD/TAIL sentinels land at file boundaries; HEAD can still collide with a
    // range starting at line 1, so it is checked like any other insertion.
    let mut consumed: Vec<(usize, usize)> = Vec::new(); // 1-indexed [start, end]
    for r in &resolved {
        let (ResolvedOp::Swap { start, end, .. } | ResolvedOp::Del { start, end }) = r else {
            continue;
        };
        for (os, oe) in &consumed {
            if *os <= *end && *start <= *oe {
                return Err(ApplyError {
                    message: format!("op range {start}..={end} overlaps existing range {os}..={oe}"),
                });
            }
        }
        consumed.push((*start, *end));
    }
    for r in &resolved {
        let ResolvedOp::Ins { pos, anchor, .. } = r else {
            continue;
        };
        let Some(p) = insertion_index(pos, *anchor) else {
            continue; // TAIL lands past the last line — no consuming range reaches it.
        };
        for (s, e) in &consumed {
            // The range consumes 0-indexed [s-1, e-1]; an insertion at p inside
            // that span would be spliced out when the consumer runs.
            if (*s - 1..=*e - 1).contains(&p) {
                return Err(ApplyError {
                    message: format!(
                        "INS (anchor {anchor:?}, {pos:?}) insertion point falls inside op range {s}..={e} and would be swallowed by that SWAP/DEL; fold the inserted content into that SWAP, or use an anchor outside the range"
                    ),
                });
            }
        }
    }

    // Apply back-to-front: highest primary line first. Insertions are ordered by
    // anchor so they don't collide with earlier deletes/swaps on the same range.
    resolved.sort_by_key(|r| std::cmp::Reverse(r.primary_line()));

    for r in resolved {
        apply_resolved(&mut lines, &r)?;
    }

    let first_changed = first_changed_line(&lines, original_len, text, ops);
    Ok(ApplyResult {
        text: lines.join("\n"),
        first_changed_line: first_changed,
    })
}

/// A concrete op with block ranges already resolved to absolute line numbers.
#[derive(Debug, Clone)]
enum ResolvedOp {
    Swap {
        start: usize,
        end: usize,
        body: Vec<String>,
    },
    Del {
        start: usize,
        end: usize,
    },
    Ins {
        pos: InsPos,
        anchor: Option<usize>,
        body: Vec<String>,
    },
}

impl ResolvedOp {
    fn primary_line(&self) -> usize {
        match self {
            ResolvedOp::Swap { start, .. } | ResolvedOp::Del { start, .. } => *start,
            ResolvedOp::Ins {
                anchor: Some(a), ..
            } => *a,
            ResolvedOp::Ins { anchor: None, .. } => 0, // HEAD sorts last (lowest)
        }
    }
}

/// 0-indexed landing position for an insertion, or `None` for TAIL (lands past
/// the last line, where no consuming range can reach). Used by the conflict
/// check to catch insertions that would be eaten by a Swap/Del on the same span.
fn insertion_index(pos: &InsPos, anchor: Option<usize>) -> Option<usize> {
    match pos {
        InsPos::Head => Some(0),
        InsPos::Tail => None,
        InsPos::Pre => anchor.map(|a| a.saturating_sub(1)),
        InsPos::Post => anchor,
    }
}

fn resolve_op(op: &Op, lines: &[String]) -> Result<ResolvedOp, ApplyError> {
    let line_refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
    match op {
        Op::Swap { start, end, body } => {
            validate_range(*start, *end, lines.len())?;
            let repaired = repair_boundaries(&line_refs, *start, *end, body);
            Ok(ResolvedOp::Swap {
                start: *start,
                end: *end,
                body: repaired,
            })
        }
        Op::Del { start, end } => {
            validate_range(*start, *end, lines.len())?;
            Ok(ResolvedOp::Del {
                start: *start,
                end: *end,
            })
        }
        Op::Ins { pos, anchor, body } => {
            if body.is_empty() {
                return Err(ApplyError {
                    message: "INS op body must not be empty".to_string(),
                });
            }
            if let Some(a) = anchor
                && (*a == 0 || *a > lines.len())
            {
                return Err(ApplyError {
                    message: format!("INS anchor line {a} out of bounds (file has {} lines)", lines.len()),
                });
            }
            Ok(ResolvedOp::Ins {
                pos: *pos,
                anchor: *anchor,
                body: body.clone(),
            })
        }
        Op::SwapBlk { start, body } => {
            let (s, e) =
                block::resolve_block_range(&line_refs, *start).map_err(|e| ApplyError {
                    message: e.to_string(),
                })?;
            let repaired = repair_boundaries(&line_refs, s, e, body);
            Ok(ResolvedOp::Swap {
                start: s,
                end: e,
                body: repaired,
            })
        }
        Op::DelBlk { start } => {
            let (s, e) =
                block::resolve_block_range(&line_refs, *start).map_err(|e| ApplyError {
                    message: e.to_string(),
                })?;
            Ok(ResolvedOp::Del { start: s, end: e })
        }
        Op::InsBlkPost { anchor, body } => {
            if body.is_empty() {
                return Err(ApplyError {
                    message: "INS.BLK.POST op body must not be empty".to_string(),
                });
            }
            let land =
                block::block_post_insertion_line(&line_refs, *anchor).map_err(|e| ApplyError {
                    message: e.to_string(),
                })?;
            // Insert AFTER `land` (1-indexed). Use Ins::Post with anchor=land.
            Ok(ResolvedOp::Ins {
                pos: InsPos::Post,
                anchor: Some(land),
                body: body.clone(),
            })
        }
    }
}

fn validate_range(start: usize, end: usize, len: usize) -> Result<(), ApplyError> {
    if start == 0 || end == 0 {
        return Err(ApplyError {
            message: "line numbers must be >= 1".to_string(),
        });
    }
    if start > end {
        return Err(ApplyError {
            message: format!("range start {start} is greater than end {end}"),
        });
    }
    if end > len {
        return Err(ApplyError {
            message: format!("range end {end} out of bounds (file has {len} lines)"),
        });
    }
    Ok(())
}

fn apply_resolved(lines: &mut Vec<String>, op: &ResolvedOp) -> Result<(), ApplyError> {
    match op {
        ResolvedOp::Swap { start, end, body } => {
            let s = start - 1;
            let e = *end; // exclusive
            lines.splice(s..e, body.iter().cloned());
        }
        ResolvedOp::Del { start, end } => {
            let s = start - 1;
            let e = *end;
            lines.drain(s..e);
        }
        ResolvedOp::Ins { pos, anchor, body } => {
            let at = match (pos, anchor) {
                (InsPos::Head, _) => 0,
                (InsPos::Tail, _) => lines.len(),
                (InsPos::Pre, Some(a)) => *a - 1,
                (InsPos::Post, Some(a)) => *a,
                _ => {
                    return Err(ApplyError {
                        message: "INS PRE/POST requires an anchor".to_string(),
                    });
                }
            };
            for (i, row) in body.iter().enumerate() {
                lines.insert(at + i, row.clone());
            }
        }
    }
    Ok(())
}

/// Boundary repair: drop body rows that restate the line immediately above
/// (`start-1`) or below (`end+1`) the range — the common model mistake of
/// echoing context. A suffix echo is dropped only when the echoed row is a
/// closer or neutral (net balance ≤ 0): an opener restated below the range is
/// still a structural opener the model asked for, so it is kept. Symmetrically,
/// a prefix echo is dropped only when the row is an opener or neutral
/// (net balance ≥ 0); a closer restated above is kept. The body is never
/// trimmed past a single row, so a fully-echoed body never silently turns a
/// SWAP into a deletion.
pub(super) fn repair_boundaries(
    lines: &[&str],
    start: usize,
    end: usize,
    body: &[String],
) -> Vec<String> {
    if body.is_empty() {
        return body.to_vec();
    }
    let mut repaired = body.to_vec();

    // Drop a trailing row that duplicates the line just below the range.
    loop {
        if repaired.len() <= 1 {
            break; // never empty the body — that would change SWAP into DEL.
        }
        let below_idx = end; // 0-indexed line after the range
        let last = repaired.last().unwrap();
        let Some(below) = lines.get(below_idx) else {
            break;
        };
        if last.as_str() == *below && net_balance(last) <= 0 {
            repaired.pop();
        } else {
            break;
        }
    }

    // Drop a leading row that duplicates the line just above the range.
    loop {
        if repaired.len() <= 1 {
            break; // never empty the body — that would change SWAP into DEL.
        }
        let first = repaired.first().unwrap();
        let above_idx = start.checked_sub(2); // 0-indexed line before the range
        let Some(above) = above_idx.and_then(|i| lines.get(i)) else {
            break;
        };
        if first.as_str() == *above && net_balance(first) >= 0 {
            repaired.remove(0);
        } else {
            break;
        }
    }

    repaired
}

/// Net `()`/`[]`/`{}` balance of a line, skipping `//` comments and string
/// literals (simplified scan — sufficient for boundary-repair heuristics).
pub(super) fn net_balance(line: &str) -> i32 {
    let mut balance: i32 = 0;
    let mut in_string = false;
    let mut string_quote = '\0';
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if in_string {
            if c == '\\' {
                chars.next();
                continue;
            }
            if c == string_quote {
                in_string = false;
            }
            continue;
        }
        match c {
            '/' if matches!(chars.peek(), Some('/')) => break,
            '"' | '\'' => {
                in_string = true;
                string_quote = c;
            }
            '(' | '[' | '{' => balance += 1,
            ')' | ']' | '}' => balance -= 1,
            _ => {}
        }
    }
    balance
}

/// First 1-indexed line that differs between the original and result, used for
/// the diff preview anchor. Falls back to 1 when the file shrank to nothing.
fn first_changed_line(new_lines: &[String], _original_len: usize, _old: &str, ops: &[Op]) -> usize {
    ops.iter()
        .map(|op| match op {
            Op::Swap { start, .. }
            | Op::Del { start, .. }
            | Op::SwapBlk { start, .. }
            | Op::DelBlk { start, .. } => *start,
            Op::Ins {
                anchor: Some(a), ..
            }
            | Op::InsBlkPost { anchor: a, .. } => *a,
            Op::Ins { anchor: None, .. } => 1,
        })
        .min()
        .unwrap_or(1)
        .max(1)
        .min(new_lines.len().max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply_str(text: &str, ops: &[Op]) -> String {
        apply(text, ops).unwrap().text
    }

    #[test]
    fn swap_single_line() {
        let ops = [Op::Swap {
            start: 2,
            end: 2,
            body: vec!["Y".into()],
        }];
        assert_eq!(apply_str("A\nB\nC", &ops), "A\nY\nC");
    }

    #[test]
    fn swap_range_grows() {
        let ops = [Op::Swap {
            start: 2,
            end: 2,
            body: vec!["X".into(), "Y".into(), "Z".into()],
        }];
        assert_eq!(apply_str("A\nB\nC", &ops), "A\nX\nY\nZ\nC");
    }

    #[test]
    fn del_range() {
        let ops = [Op::Del { start: 2, end: 3 }];
        assert_eq!(apply_str("A\nB\nC\nD", &ops), "A\nD");
    }

    #[test]
    fn ins_pre_post_head_tail() {
        let ops = [
            Op::Ins {
                pos: InsPos::Head,
                anchor: None,
                body: vec!["H".into()],
            },
            Op::Ins {
                pos: InsPos::Tail,
                anchor: None,
                body: vec!["T".into()],
            },
            Op::Ins {
                pos: InsPos::Pre,
                anchor: Some(2),
                body: vec!["PRE".into()],
            },
            Op::Ins {
                pos: InsPos::Post,
                anchor: Some(2),
                body: vec!["POST".into()],
            },
        ];
        // Anchors reference the ORIGINAL file (A=1, B=2, C=3); applied
        // back-to-front: POST 2, PRE 2, HEAD, TAIL … but PRE/POST share anchor
        // 2 and stable-sort keeps PRE before POST. Net result:
        assert_eq!(apply_str("A\nB\nC", &ops), "H\nA\nPRE\nPOST\nB\nC\nT");
    }

    #[test]
    fn multiple_hunks_keep_line_numbers() {
        // Two SWAPs at lines 1 and 3 — both reference original numbering.
        let ops = [
            Op::Swap {
                start: 1,
                end: 1,
                body: vec!["X".into()],
            },
            Op::Swap {
                start: 3,
                end: 3,
                body: vec!["Z".into()],
            },
        ];
        assert_eq!(apply_str("A\nB\nC", &ops), "X\nB\nZ");
    }

    #[test]
    fn overlapping_ranges_rejected() {
        let ops = [
            Op::Swap {
                start: 1,
                end: 3,
                body: vec!["X".into()],
            },
            Op::Swap {
                start: 2,
                end: 4,
                body: vec!["Y".into()],
            },
        ];
        assert!(apply("A\nB\nC\nD", &ops).is_err());
    }

    #[test]
    fn boundary_repair_drops_repeated_below() {
        // Body restates line 3 (`C`) below the range — should be dropped.
        let ops = [Op::Swap {
            start: 2,
            end: 2,
            body: vec!["NEW".into(), "C".into()],
        }];
        assert_eq!(apply_str("A\nB\nC", &ops), "A\nNEW\nC");
    }

    #[test]
    fn boundary_repair_drops_repeated_above() {
        // Body restates line 1 (`A`) above the range — should be dropped.
        let ops = [Op::Swap {
            start: 2,
            end: 2,
            body: vec!["A".into(), "NEW".into()],
        }];
        assert_eq!(apply_str("A\nB\nC", &ops), "A\nNEW\nC");
    }

    #[test]
    fn boundary_repair_keeps_unbalanced() {
        // A `{` opener restated below the range has net balance > 0, so it must
        // NOT be dropped — the model asked for a structural opener there.
        let ops = [Op::Swap {
            start: 2,
            end: 2,
            body: vec!["{".into()],
        }];
        assert_eq!(apply_str("A\nB\n{", &ops), "A\n{\n{");
    }

    #[test]
    fn swap_blk_replaces_function() {
        let src = "fn a() {\n    x();\n}\nfn b() {}";
        let ops = [Op::SwapBlk {
            start: 1,
            body: vec!["fn a() {".into(), "    y();".into(), "}".into()],
        }];
        assert_eq!(apply_str(src, &ops), "fn a() {\n    y();\n}\nfn b() {}");
    }

    #[test]
    fn del_blk_deletes_function() {
        let src = "fn a() {\n    x();\n}\nfn b() {}";
        let ops = [Op::DelBlk { start: 1 }];
        assert_eq!(apply_str(src, &ops), "fn b() {}");
    }

    #[test]
    fn ins_blk_post_after_function() {
        let src = "fn a() {\n    x();\n}\nfn b() {}";
        let ops = [Op::InsBlkPost {
            anchor: 1,
            body: vec!["// trail".into()],
        }];
        assert_eq!(
            apply_str(src, &ops),
            "fn a() {\n    x();\n}\n// trail\nfn b() {}"
        );
    }

    #[test]
    fn range_out_of_bounds_rejected() {
        let ops = [Op::Swap {
            start: 1,
            end: 99,
            body: vec!["X".into()],
        }];
        assert!(apply("A\nB", &ops).is_err());
    }

    #[test]
    fn empty_ins_rejected() {
        let ops = [Op::Ins {
            pos: InsPos::Pre,
            anchor: Some(1),
            body: vec![],
        }];
        assert!(apply("A", &ops).is_err());
    }

    #[test]
    fn ins_pre_inside_swap_range_rejected() {
        // INS.PRE 5 lands at the start of SWAP 5.=5's consumed span; applying
        // it back-to-front would let the swap eat the inserted rows. Reject
        // rather than silently drop them.
        let ops = [
            Op::Ins {
                pos: InsPos::Pre,
                anchor: Some(5),
                body: vec!["X".into()],
            },
            Op::Swap {
                start: 5,
                end: 5,
                body: vec!["Y".into()],
            },
        ];
        assert!(apply("A\nB\nC\nD\nE", &ops).is_err());
    }

    #[test]
    fn ins_post_inside_swap_range_rejected() {
        // SWAP 5.=6 consumes indices 4..6; INS.POST 5 lands at index 5, inside
        // that span — reject.
        let ops = [
            Op::Ins {
                pos: InsPos::Post,
                anchor: Some(5),
                body: vec!["Z".into()],
            },
            Op::Swap {
                start: 5,
                end: 6,
                body: vec!["Y".into()],
            },
        ];
        assert!(apply("A\nB\nC\nD\nE\nF", &ops).is_err());
    }

    #[test]
    fn ins_post_adjacent_to_single_line_swap_ok() {
        // INS.POST 5 lands at index 5, just past SWAP 5.=5's consumed index 4 —
        // no collision, both apply.
        let ops = [
            Op::Ins {
                pos: InsPos::Post,
                anchor: Some(5),
                body: vec!["Z".into()],
            },
            Op::Swap {
                start: 5,
                end: 5,
                body: vec!["Y".into()],
            },
        ];
        assert_eq!(apply_str("A\nB\nC\nD\nE", &ops), "A\nB\nC\nD\nY\nZ");
    }

    #[test]
    fn boundary_repair_never_empties_body() {
        // Every body row echoes the line below the range; repair trims echoes
        // but keeps at least one row so SWAP does not silently become DEL.
        let ops = [Op::Swap {
            start: 2,
            end: 2,
            body: vec!["}".into(), "}".into()],
        }];
        // File line 3 is `}` (below the range). Both body rows echo it; the last
        // is dropped, the remaining `}` stays rather than emptying the body.
        assert_eq!(apply_str("A\nB\n}\n", &ops), "A\n}\n}");
    }
}
