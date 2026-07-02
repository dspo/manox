//! Hashline patch parser.
//!
//! Parses a patch text into a list of `FilePatch` sections. The grammar is a
//! line-oriented state machine mirroring oh-my-pi's `grammar.lark`:
//!
//! ```text
//! *** Begin Patch            (optional envelope open)
//! [PATH#TAG]                 (file section header)
//! SWAP N.=M: / SWAP N:       (replace range, body follows)
//! SWAP.BLK N:                (replace bracket-block, body follows)
//! DEL N.=M / DEL N           (delete range, no body)
//! DEL.BLK N                  (delete bracket-block, no body)
//! INS.PRE N: / INS.POST N:   (insert before/after anchor, body follows)
//! INS.HEAD: / INS.TAIL:      (insert at start/end, body follows)
//! INS.BLK.POST N:            (insert after bracket-block, body follows)
//! +TEXT                      (body row; `+` alone = blank line;
//!                            `+-x`/`++x` escapes a literal `-`/`+` lead)
//! *** End Patch              (optional envelope close)
//! ```
//!
//! Line numbers are 1-indexed, non-zero, no leading zeros. Ranges are inclusive
//! on both ends. Only body-bearing headers end in `:`; `DEL` has no body.

use std::path::PathBuf;

/// Insertion position for `INS` ops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsPos {
    Pre,
    Post,
    Head,
    Tail,
}

/// A single parsed operation against a file's line array.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// `SWAP N.=M:` — replace inclusive lines `start..=end` with `body`.
    Swap {
        start: usize,
        end: usize,
        body: Vec<String>,
    },
    /// `DEL N.=M` / `DEL N` — delete inclusive lines `start..=end`.
    Del { start: usize, end: usize },
    /// `INS.PRE N:` / `INS.POST N:` / `INS.HEAD:` / `INS.TAIL:` — insert `body`.
    Ins {
        pos: InsPos,
        anchor: Option<usize>,
        body: Vec<String>,
    },
    /// `SWAP.BLK N:` — resolve bracket-block at `start`, replace its span.
    SwapBlk { start: usize, body: Vec<String> },
    /// `DEL.BLK N` — resolve bracket-block at `start`, delete its span.
    DelBlk { start: usize },
    /// `INS.BLK.POST N:` — insert after the bracket-block at `anchor`.
    InsBlkPost { anchor: usize, body: Vec<String> },
}

/// One file section: a path, the snapshot tag it claims, and its operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePatch {
    pub path: PathBuf,
    pub tag: String,
    pub ops: Vec<Op>,
}

/// Parse failure carrying the 1-indexed line and a reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub message: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "patch 第 {} 行解析失败: {}", self.line, self.message)
    }
}

impl std::error::Error for ParseError {}

/// Parse a patch text into file sections.
pub fn parse_patch(text: &str) -> Result<Vec<FilePatch>, ParseError> {
    let mut sections: Vec<FilePatch> = Vec::new();
    // Current body accumulator and whether it feeds the last op of the section.
    let mut body: Vec<String> = Vec::new();
    let mut pending_body = false;

    for (idx, raw) in text.lines().enumerate() {
        let line_no = idx + 1;
        let line = raw.trim_end_matches('\r');

        // Optional envelope markers — accepted anywhere, consumed silently.
        if line == "*** Begin Patch" || line == "*** End Patch" {
            continue;
        }

        // Body rows belong to the pending body-bearing op.
        if pending_body {
            if let Some(rest) = strip_body_prefix(line) {
                body.push(rest);
                continue;
            }
            // No `+` prefix → flush the pending body, then fall through to parse
            // this line as a new header / section.
            flush_body(&mut sections, &mut body, &mut pending_body)?;
        }

        if line.is_empty() {
            continue;
        }

        if let Some(section) = parse_section_header(line) {
            if pending_body {
                flush_body(&mut sections, &mut body, &mut pending_body)?;
            }
            sections.push(section);
            continue;
        }

        match parse_op_header(line) {
            Ok(ParsedOp::BodyBearer(op)) => {
                flush_body(&mut sections, &mut body, &mut pending_body)?;
                push_op(&mut sections, op);
                pending_body = true;
            }
            Ok(ParsedOp::Bodyless(op)) => {
                flush_body(&mut sections, &mut body, &mut pending_body)?;
                push_op(&mut sections, op);
            }
            Err(msg) => {
                return Err(ParseError {
                    line: line_no,
                    message: msg,
                });
            }
        }
    }

    // Flush any trailing body.
    flush_body(&mut sections, &mut body, &mut pending_body)?;

    if sections.is_empty() {
        return Err(ParseError {
            line: 0,
            message: "patch 不含任何 [PATH#TAG] 段".to_string(),
        });
    }
    Ok(sections)
}

fn push_op(sections: &mut [FilePatch], op: Op) {
    if let Some(sec) = sections.last_mut() {
        sec.ops.push(op);
    }
}

/// Flush the pending body into the body-bearing op it belongs to.
fn flush_body(
    sections: &mut [FilePatch],
    body: &mut Vec<String>,
    pending: &mut bool,
) -> Result<(), ParseError> {
    if !*pending {
        if body.is_empty() {
            return Ok(());
        }
        return Err(ParseError {
            line: 0,
            message: "body 行无对应 `:` 头".to_string(),
        });
    }
    // Body-bearing header with no rows is allowed for SWAP (clears range), but
    // INS/INS.BLK.POST require at least one row — validated later in apply.
    if let Some(sec) = sections.last_mut()
        && let Some(last) = sec.ops.last_mut()
    {
        match last {
            Op::Swap { body: b, .. }
            | Op::SwapBlk { body: b, .. }
            | Op::Ins { body: b, .. }
            | Op::InsBlkPost { body: b, .. } => {
                b.append(body);
            }
            Op::Del { .. } | Op::DelBlk { .. } => {}
        }
    }
    body.clear();
    *pending = false;
    Ok(())
}

/// `+TEXT` → `TEXT`; `+` alone → `""`; `+-x`/`++x` → `-x`/`+x`. Non-`+` lines
/// return `None` (not a body row).
fn strip_body_prefix(line: &str) -> Option<String> {
    let rest = line.strip_prefix('+')?;
    // Escape: `+-` → `-`, `++` → `+`. Any other char after `+` is verbatim.
    if let Some(escaped) = rest
        .strip_prefix('-')
        .map(|r| format!("-{r}"))
        .or_else(|| rest.strip_prefix('+').map(|r| format!("+{r}")))
    {
        // Only treat as escape when the second char is `-` or `+`; otherwise the
        // first char is literal content (e.g. `+    code` → `    code`).
        if rest.starts_with('-') || rest.starts_with('+') {
            return Some(escaped);
        }
    }
    Some(rest.to_string())
}

/// A parsed op header: either body-bearing (needs `+` rows) or bodyless.
#[derive(Debug)]
enum ParsedOp {
    BodyBearer(Op),
    Bodyless(Op),
}

fn parse_op_header(line: &str) -> Result<ParsedOp, String> {
    // SWAP.BLK N:
    if let Some(rest) = line.strip_prefix("SWAP.BLK ") {
        let (n, tail) = parse_lid(rest)?;
        expect_colon(tail)?;
        return Ok(ParsedOp::BodyBearer(Op::SwapBlk {
            start: n,
            body: Vec::new(),
        }));
    }
    // DEL.BLK N
    if let Some(rest) = line.strip_prefix("DEL.BLK ") {
        let (n, tail) = parse_lid(rest)?;
        expect_eol(tail)?;
        return Ok(ParsedOp::Bodyless(Op::DelBlk { start: n }));
    }
    // INS.BLK.POST N:
    if let Some(rest) = line.strip_prefix("INS.BLK.POST ") {
        let (n, tail) = parse_lid(rest)?;
        expect_colon(tail)?;
        return Ok(ParsedOp::BodyBearer(Op::InsBlkPost {
            anchor: n,
            body: Vec::new(),
        }));
    }
    // SWAP N.=M: / SWAP N:
    if let Some(rest) = line.strip_prefix("SWAP ") {
        let (start, end, tail) = parse_range(rest)?;
        expect_colon(tail)?;
        return Ok(ParsedOp::BodyBearer(Op::Swap {
            start,
            end,
            body: Vec::new(),
        }));
    }
    // DEL N.=M / DEL N
    if let Some(rest) = line.strip_prefix("DEL ") {
        let (start, end, tail) = parse_range(rest)?;
        expect_eol(tail)?;
        return Ok(ParsedOp::Bodyless(Op::Del { start, end }));
    }
    // INS.PRE N: / INS.POST N: / INS.HEAD: / INS.TAIL:
    if let Some(rest) = line.strip_prefix("INS.") {
        if let Some(rest) = rest.strip_prefix("PRE ") {
            let (n, tail) = parse_lid(rest)?;
            expect_colon(tail)?;
            return Ok(ParsedOp::BodyBearer(Op::Ins {
                pos: InsPos::Pre,
                anchor: Some(n),
                body: Vec::new(),
            }));
        }
        if let Some(rest) = rest.strip_prefix("POST ") {
            let (n, tail) = parse_lid(rest)?;
            expect_colon(tail)?;
            return Ok(ParsedOp::BodyBearer(Op::Ins {
                pos: InsPos::Post,
                anchor: Some(n),
                body: Vec::new(),
            }));
        }
        if let Some(rest) = rest.strip_prefix("HEAD") {
            expect_colon(rest)?;
            return Ok(ParsedOp::BodyBearer(Op::Ins {
                pos: InsPos::Head,
                anchor: None,
                body: Vec::new(),
            }));
        }
        if let Some(rest) = rest.strip_prefix("TAIL") {
            expect_colon(rest)?;
            return Ok(ParsedOp::BodyBearer(Op::Ins {
                pos: InsPos::Tail,
                anchor: None,
                body: Vec::new(),
            }));
        }
        return Err("未知 INS 位置（应为 PRE/POST/HEAD/TAIL）".to_string());
    }
    Err(format!("无法识别的操作头: {line:?}"))
}

/// Parse `[PATH#TAG]` → `FilePatch` with empty ops. Returns `None` if not a
/// section header.
fn parse_section_header(line: &str) -> Option<FilePatch> {
    let inner = line.strip_prefix('[')?.strip_suffix(']')?;
    let hash_sep = inner.rfind('#')?;
    let (path_part, tag) = inner.split_at(hash_sep);
    let tag = &tag[1..]; // skip '#'
    if !is_valid_tag(tag) {
        return None;
    }
    let path = unquote_path(path_part);
    Some(FilePatch {
        path: PathBuf::from(path),
        tag: tag.to_string(),
        ops: Vec::new(),
    })
}

fn is_valid_tag(tag: &str) -> bool {
    tag.len() == 4 && tag.chars().all(|c| matches!(c, '0'..='9' | 'A'..='F'))
}

fn unquote_path(s: &str) -> &str {
    let s = s.trim();
    if (s.starts_with('"') && s.ends_with('"') && s.len() >= 2)
        || (s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2)
    {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// Parse a `N.=M` range (or bare `N`), returning `(start, end, remaining)`.
fn parse_range(s: &str) -> Result<(usize, usize, &str), String> {
    let (start, rest) = parse_lid(s)?;
    if let Some(rest) = rest.strip_prefix(".=") {
        let (end, tail) = parse_lid(rest)?;
        if end < start {
            return Err(format!("范围结束 {end} 小于起始 {start}"));
        }
        return Ok((start, end, tail));
    }
    Ok((start, start, rest))
}

/// Parse a 1-indexed line id (`[1-9]\d*`), returning `(n, remaining)`.
fn parse_lid(s: &str) -> Result<(usize, &str), String> {
    let s = s.strip_prefix(' ').unwrap_or(s);
    let mut end = 0;
    let bytes = s.as_bytes();
    if bytes.is_empty() || !bytes[0].is_ascii_digit() || bytes[0] == b'0' {
        return Err(format!("期望行号（非零数字开头）: {s:?}"));
    }
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    let n: usize = s[..end].parse().map_err(|_| format!("行号溢出: {s:?}"))?;
    Ok((n, &s[end..]))
}

fn expect_colon(s: &str) -> Result<(), String> {
    let s = s.strip_prefix(' ').unwrap_or(s);
    if s == ":" {
        Ok(())
    } else {
        Err(format!("期望 `:` 结尾，遇到 {:?}", s))
    }
}

fn expect_eol(s: &str) -> Result<(), String> {
    let s = s.trim();
    if s.is_empty() {
        Ok(())
    } else {
        Err(format!("期望行尾，遇到 {:?}", s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op_at(sections: &[FilePatch], i: usize) -> &Op {
        &sections[0].ops[i]
    }

    #[test]
    fn parses_swap_range() {
        let p = parse_patch("[a.rs#1A2B]\nSWAP 2.=3:\n+x\n+y").unwrap();
        assert_eq!(
            op_at(&p, 0),
            &Op::Swap {
                start: 2,
                end: 3,
                body: vec!["x".into(), "y".into()]
            }
        );
    }

    #[test]
    fn parses_swap_single_as_range() {
        let p = parse_patch("[a.rs#1A2B]\nSWAP 5:\n+z").unwrap();
        assert_eq!(
            op_at(&p, 0),
            &Op::Swap {
                start: 5,
                end: 5,
                body: vec!["z".into()]
            }
        );
    }

    #[test]
    fn parses_del_range_and_single() {
        let p = parse_patch("[a.rs#1A2B]\nDEL 2.=4\nDEL 7").unwrap();
        assert_eq!(op_at(&p, 0), &Op::Del { start: 2, end: 4 });
        assert_eq!(op_at(&p, 1), &Op::Del { start: 7, end: 7 });
    }

    #[test]
    fn parses_ins_variants() {
        let p = parse_patch(
            "[a.rs#1A2B]\nINS.PRE 2:\n+x\nINS.POST 3:\n+y\nINS.HEAD:\n+h\nINS.TAIL:\n+t",
        )
        .unwrap();
        assert!(matches!(
            op_at(&p, 0),
            Op::Ins {
                pos: InsPos::Pre,
                anchor: Some(2),
                ..
            }
        ));
        assert!(matches!(
            op_at(&p, 1),
            Op::Ins {
                pos: InsPos::Post,
                anchor: Some(3),
                ..
            }
        ));
        assert!(matches!(
            op_at(&p, 2),
            Op::Ins {
                pos: InsPos::Head,
                anchor: None,
                ..
            }
        ));
        assert!(matches!(
            op_at(&p, 3),
            Op::Ins {
                pos: InsPos::Tail,
                anchor: None,
                ..
            }
        ));
    }

    #[test]
    fn parses_block_ops() {
        let p =
            parse_patch("[a.rs#1A2B]\nSWAP.BLK 1:\n+x\nDEL.BLK 2\nINS.BLK.POST 3:\n+y").unwrap();
        assert!(matches!(op_at(&p, 0), Op::SwapBlk { start: 1, .. }));
        assert!(matches!(op_at(&p, 1), Op::DelBlk { start: 2 }));
        assert!(matches!(op_at(&p, 2), Op::InsBlkPost { anchor: 3, .. }));
    }

    #[test]
    fn body_blank_and_escape() {
        let p = parse_patch("[a.rs#1A2B]\nSWAP 1.=1:\n+\n+-x\n++y").unwrap();
        assert_eq!(
            op_at(&p, 0),
            &Op::Swap {
                start: 1,
                end: 1,
                body: vec!["".into(), "-x".into(), "+y".into()]
            }
        );
    }

    #[test]
    fn body_preserves_leading_whitespace() {
        let p = parse_patch("[a.rs#1A2B]\nSWAP 1.=1:\n+    code").unwrap();
        assert_eq!(
            op_at(&p, 0),
            &Op::Swap {
                start: 1,
                end: 1,
                body: vec!["    code".into()]
            }
        );
    }

    #[test]
    fn envelope_optional() {
        let with = parse_patch("*** Begin Patch\n[a.rs#1A2B]\nDEL 1\n*** End Patch").unwrap();
        let without = parse_patch("[a.rs#1A2B]\nDEL 1").unwrap();
        assert_eq!(with, without);
    }

    #[test]
    fn multiple_sections() {
        let p = parse_patch("[a.rs#1A2B]\nDEL 1\n[b.rs#3C4D]\nSWAP 1.=1:\n+x").unwrap();
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].path, PathBuf::from("a.rs"));
        assert_eq!(p[1].path, PathBuf::from("b.rs"));
    }

    #[test]
    fn error_on_bad_tag() {
        assert!(parse_patch("[a.rs#bad]\nDEL 1").is_err());
    }

    #[test]
    fn error_on_zero_line() {
        assert!(parse_patch("[a.rs#1A2B]\nDEL 0").is_err());
    }

    #[test]
    fn error_on_unrecognized_header() {
        let e = parse_patch("[a.rs#1A2B]\nFROB 1").unwrap_err();
        assert!(e.message.contains("无法识别"));
    }
}
