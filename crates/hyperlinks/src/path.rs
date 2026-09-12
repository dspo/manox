//! Filesystem-path detection with optional `path:line:col` anchors.

use std::path::PathBuf;

use crate::shared::{OverlaySpan, UrlKind};

/// Tuning knobs for path detection.
#[derive(Debug, Clone, Default)]
pub struct PathOptions {
    /// Resolve relative candidates against this directory; a candidate that
    /// exists under it is a path even without an extension or line anchor.
    pub cwd: Option<PathBuf>,
    /// Whether a trailing `:N`, `:N-M`, or `:N:M` line anchor counts as a
    /// path. The anchor is part of the span (`foo.rs:42` links the whole).
    pub enable_line_col: bool,
    /// Extension whitelist on top of the generic extension heuristic (a
    /// non-empty 1..=10 ASCII-alphanumeric suffix after the last dot).
    pub known_extensions: Vec<&'static str>,
}

/// Detect every path-like span in `text`. Candidates are collected from word
/// boundaries (or a bare `/`, `./`, `../` start) and validated by
/// [`is_path_like`]. Spans are non-overlapping by construction.
pub fn detect_paths(text: &str, opts: &PathOptions) -> Vec<OverlaySpan> {
    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut spans = Vec::new();
    let mut i = 0;
    while i < len {
        // A candidate may start anywhere after a delimiter, or mid-text only
        // when it begins with a path separator form.
        let is_boundary = i == 0
            || matches!(
                bytes[i - 1],
                b' ' | b'\t' | b'\n' | b'(' | b'[' | b'"' | b'\''
            );
        if !is_boundary
            && !(bytes[i] == b'/'
                || (i + 1 < len && bytes[i] == b'.' && bytes[i + 1] == b'/')
                || (i + 2 < len
                    && bytes[i] == b'.'
                    && bytes[i + 1] == b'.'
                    && bytes[i + 2] == b'/'))
        {
            i += 1;
            continue;
        }
        let Some(end) = collect_candidate(bytes, i) else {
            i += 1;
            continue;
        };
        let candidate = &text[i..end];
        if is_path_like(candidate, opts) {
            spans.push(OverlaySpan {
                href: candidate.to_string(),
                range: i..end,
                kind: UrlKind::Path,
            });
        }
        i = end;
    }
    spans
}

/// Collect a maximal run of path-safe characters starting at `pos`. Returns
/// `None` when the run contains no `/` (a bare filename is not a path).
fn collect_candidate(bytes: &[u8], pos: usize) -> Option<usize> {
    let mut end = pos;
    while end < bytes.len() {
        let b = bytes[end];
        if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'@' | b'/' | b':') {
            end += 1;
        } else {
            break;
        }
    }
    bytes[pos..end].contains(&b'/').then_some(end)
}

/// Whether `candidate` is an openable path: it contains a `/` and at least
/// one of — a recognized extension (heuristic or whitelist), a `:line(:col)`
/// anchor, or existence under `opts.cwd` (absolute candidates checked
/// directly).
pub fn is_path_like(candidate: &str, opts: &PathOptions) -> bool {
    if !candidate.contains('/') {
        return false;
    }
    if opts.enable_line_col && has_line_anchor(candidate) {
        return true;
    }
    if let Some(ext) = extension(candidate)
        && (heuristic_extension(ext) || opts.known_extensions.contains(&ext))
    {
        return true;
    }
    if let Some(cwd) = &opts.cwd {
        let p = if candidate.starts_with('/') {
            PathBuf::from(candidate)
        } else {
            cwd.join(candidate)
        };
        if p.exists() {
            return true;
        }
    }
    false
}

/// The suffix after the last `/`-separated dot, `None` when there is none.
fn extension(candidate: &str) -> Option<&str> {
    let dot = candidate.rfind('.')?;
    let ext = &candidate[dot + 1..];
    (!ext.is_empty() && !ext.contains('/')).then_some(ext)
}

fn heuristic_extension(ext: &str) -> bool {
    (1..=10).contains(&ext.len()) && ext.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// A trailing `:N`, `:N-M`, or `:N:M` line anchor (digits, optional `-end`
/// range, optional `:col`).
fn has_line_anchor(candidate: &str) -> bool {
    let Some(colon) = candidate.rfind(':') else {
        return false;
    };
    let after = &candidate[colon + 1..];
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if let Some((line, col)) = after.split_once(':') {
        return digits(line) && digits(col);
    }
    if let Some((line, end)) = after.split_once('-') {
        return digits(line) && digits(end);
    }
    digits(after)
}

/// A helper `PathOptions` for callers without a working directory: anchor
/// suffix on, no cwd checks, no extension whitelist.
pub fn default_path_options() -> PathOptions {
    PathOptions {
        cwd: None,
        enable_line_col: true,
        known_extensions: Vec::new(),
    }
}
