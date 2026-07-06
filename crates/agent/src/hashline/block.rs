//! Bracket-balance block range resolution for `.BLK` ops.
//!
//! A "block" is the multi-line construct that begins on the anchor line and
//! extends to the line where `()` / `[]` / `{}` net balance first returns to
//! zero. This is a tree-sitter-free heuristic: it covers curly-brace languages
//! (Rust, TS/JS, Go, C/C++, Java, JSON, CSS) without bundling per-language
//! grammar binaries into the single-binary deliverable.
//!
//! Single-line statements with no bracket open/close resolve to one line and are
//! rejected as `NotABlock`. Indentation-based languages (Python) without bracket
//! block syntax are rejected as `UnsupportedLanguage` so the caller can fall
//! back to an explicit `SWAP N.=M` range.

/// A block-resolution failure carrying a hint the caller can surface to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockError {
    /// The anchor line is a single-line statement with no bracket open/close —
    /// point at the real opener or use a plain `SWAP N.=N` / `DEL N`.
    NotABlock { line: usize },
    /// The anchor opens a bracket-block but the file ended with the balance still
    /// non-zero (unterminated construct).
    Unterminated { line: usize },
    /// The language has no bracket-block syntax at the anchor (e.g. Python); use
    /// an explicit `SWAP N.=M` range instead.
    UnsupportedLanguage { line: usize },
}

impl std::fmt::Display for BlockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BlockError::NotABlock { line } => write!(
                f,
                "line {line} is not the start of a multi-line block (no bracket pairing); use SWAP {line}.={line}: or point at the real block-start line"
            ),
            BlockError::Unterminated { line } => {
                write!(f, "block starting at line {line} is unterminated (brackets unbalanced to EOF)")
            }
            BlockError::UnsupportedLanguage { line } => write!(
                f,
                "the language of line {line} has no bracket-block syntax (e.g. Python); use SWAP N.=M with an explicit range"
            ),
        }
    }
}

impl std::error::Error for BlockError {}

/// Resolve the inclusive `(start, end)` line range of the bracket-block whose
/// opening begins on `start_line` (1-indexed). Returns the range on success.
pub fn resolve_block_range(
    lines: &[&str],
    start_line: usize,
) -> Result<(usize, usize), BlockError> {
    if start_line == 0 || start_line > lines.len() {
        return Err(BlockError::NotABlock { line: start_line });
    }
    let start_idx = start_line - 1;

    // Skip leading blank/comment lines to find the real block opener at or after
    // the anchor (the model may point at a decorator/attribute line; callers
    // that want the decorator swept should anchor on it directly).
    let opener_idx = (start_idx..lines.len())
        .find(|&i| is_significant(lines[i]))
        .ok_or(BlockError::NotABlock { line: start_line })?;

    let opener_net = net_balance(lines[opener_idx]);
    if opener_net <= 0 {
        // A block opener must open brackets on its first line. A bare closer
        // (net < 0) or a net-neutral line is not a block start; the latter may
        // still be an indentation-language block, which gets a more specific error.
        if opener_net == 0 && looks_like_indent_block(lines, opener_idx) {
            return Err(BlockError::UnsupportedLanguage {
                line: opener_idx + 1,
            });
        }
        return Err(BlockError::NotABlock {
            line: opener_idx + 1,
        });
    }

    let mut balance = opener_net;
    for (i, line) in lines.iter().enumerate().skip(opener_idx + 1) {
        balance += net_balance(line);
        if balance == 0 {
            return Ok((opener_idx + 1, i + 1));
        }
    }
    Err(BlockError::Unterminated {
        line: opener_idx + 1,
    })
}

/// The landing line for `INS.BLK.POST`: the block's end line advanced past any
/// trailing pure-closer lines so a shallower body lands at sibling depth.
pub fn block_post_insertion_line(lines: &[&str], start_line: usize) -> Result<usize, BlockError> {
    let (_, end) = resolve_block_range(lines, start_line)?;
    let mut land = end; // 1-indexed; insert after `end`
    // Slide forward across consecutive pure-closer lines (`)`, `};`, etc.) so a
    // body whose indent is shallower than the block's interior still lands outside.
    while land < lines.len() && is_pure_closer(lines[land]) {
        land += 1;
    }
    Ok(land)
}

/// Net `()`/`[]`/`{}` balance of a line, scanning char by char while skipping
/// single-line `//` comments and string literals. Multi-line block comments and
/// raw strings are not fully tokenized — this matches the simplified scan used
/// by oh-my-pi's boundary repair, which is sufficient for block-range heuristics.
fn net_balance(line: &str) -> i32 {
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
            '/' if matches!(chars.peek(), Some('/')) => break, // line comment
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

fn is_significant(line: &str) -> bool {
    let t = line.trim();
    !t.is_empty() && !t.starts_with("//")
}

/// Heuristic for indentation-language blocks: a non-empty line ending in `:`
/// (Python) or `then` (shell) with no bracket open.
fn looks_like_indent_block(lines: &[&str], idx: usize) -> bool {
    let t = lines[idx].trim_end();
    if t.ends_with(':') && net_balance(t) == 0 {
        // The next non-blank line should be more indented than this one.
        let base = leading_indent(t);
        if let Some(next) = lines.iter().skip(idx + 1).find(|l| !l.trim().is_empty()) {
            return leading_indent(next.trim_end()) > base;
        }
    }
    false
}

fn leading_indent(s: &str) -> usize {
    s.chars().take_while(|c| *c == ' ' || *c == '\t').count()
}

fn is_pure_closer(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() {
        return false;
    }
    // `)`, `}`, `];`, `})`, etc. — only closing brackets and trailing punctuation.
    t.chars().all(|c| matches!(c, ')' | ']' | '}' | ',' | ';'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &str) -> Vec<&str> {
        s.lines().collect()
    }

    #[test]
    fn rust_fn_block() {
        let src = lines("fn main() {\n    let x = 1;\n    println!(x);\n}\nfn other() {}");
        let (start, end) = resolve_block_range(&src, 1).unwrap();
        assert_eq!((start, end), (1, 4));
    }

    #[test]
    fn nested_block_resolves_outer() {
        let src = lines("fn a() {\n    if x {\n        y();\n    }\n    z();\n}");
        let (start, end) = resolve_block_range(&src, 1).unwrap();
        assert_eq!((start, end), (1, 6));
    }

    #[test]
    fn nested_block_resolves_inner() {
        let src = lines("fn a() {\n    if x {\n        y();\n    }\n    z();\n}");
        let (start, end) = resolve_block_range(&src, 2).unwrap();
        assert_eq!((start, end), (2, 4));
    }

    #[test]
    fn single_line_statement_rejected() {
        let src = lines("let x = 1;\nlet y = 2;");
        assert_eq!(
            resolve_block_range(&src, 1),
            Err(BlockError::NotABlock { line: 1 })
        );
    }

    #[test]
    fn unterminated_rejected() {
        let src = lines("fn a() {\n    let x = 1;");
        assert_eq!(
            resolve_block_range(&src, 1),
            Err(BlockError::Unterminated { line: 1 })
        );
    }

    #[test]
    fn python_indent_block_unsupported() {
        let src = lines("def greet(name):\n    print(name)\n    return");
        assert_eq!(
            resolve_block_range(&src, 1),
            Err(BlockError::UnsupportedLanguage { line: 1 })
        );
    }

    #[test]
    fn block_post_insertion_skips_closers() {
        let src = lines("fn a() {\n    x();\n}\n\nfn b() {}");
        let land = block_post_insertion_line(&src, 1).unwrap();
        // End is line 3 (`}`); insert after it → line 4.
        assert_eq!(land, 3);
    }

    #[test]
    fn bare_closer_anchor_rejected() {
        // Anchoring `.BLK` on a bare `}` (net < 0) must not scan forward and
        // mistake `}` + the next opener as a block.
        let src = lines("}\nfn a() {\n    x();\n}");
        assert_eq!(
            resolve_block_range(&src, 1),
            Err(BlockError::NotABlock { line: 1 })
        );
    }
}
