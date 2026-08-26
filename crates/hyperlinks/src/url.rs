//! URL detection: multi-protocol scheme scan + boundary trimming.

use crate::shared::{OverlaySpan, UrlKind};

/// Recognized URL schemes, longest prefix first so `https://` wins over
/// `http:` inside a scan position. Schemes without `//` (mailto, ssh, …)
/// still require content after the prefix to match.
pub const SCHEMES: &[&str] = &[
    "ipfs:",
    "ipns:",
    "magnet:",
    "mailto:",
    "gemini://",
    "gopher://",
    "https://",
    "http://",
    "news:",
    "file://",
    "git://",
    "ssh:",
    "ftp://",
    "zed://",
];

/// Characters that end a URL scan. Unlike trailing punctuation they are never
/// part of the link, even mid-word (angle brackets, quotes, braces).
const TERMINATORS: &[u8] = b"<>\"{}|\\`^'";

/// Detect every URL in `text` and return its spans. Non-overlapping by
/// construction — the scan resumes after each trimmed match.
pub fn detect_urls(text: &str) -> Vec<OverlaySpan> {
    let bytes = text.as_bytes();
    let mut spans = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let rest = &bytes[i..];
        let Some(scheme) = SCHEMES.iter().find(|s| rest.starts_with(s.as_bytes())) else {
            i += 1;
            continue;
        };
        let start = i;
        let mut end = i + scheme.len();
        while end < bytes.len() && !is_terminator(bytes[end]) {
            end += 1;
        }
        let raw = &text[start..end];
        let trimmed = trim_url(raw);
        // An empty host / empty payload after the scheme is not a link.
        if trimmed.len() > scheme.len() {
            spans.push(OverlaySpan {
                href: trimmed.to_string(),
                range: start..start + trimmed.len(),
                kind: UrlKind::Url,
            });
            i = start + trimmed.len();
        } else {
            i = start + scheme.len();
        }
    }
    spans
}

fn is_terminator(b: u8) -> bool {
    b.is_ascii_whitespace() || b.is_ascii_control() || TERMINATORS.contains(&b)
}

/// Trim a raw URL candidate to its link extent: control characters off both
/// ends, then trailing punctuation and unmatched closing brackets.
pub fn trim_url(raw: &str) -> &str {
    let mut start = 0;
    let mut end = raw.len();
    while start < end {
        let b = raw.as_bytes()[start];
        if b.is_ascii_whitespace() || b.is_ascii_control() {
            start += 1;
        } else {
            break;
        }
    }
    while end > start {
        let b = raw.as_bytes()[end - 1];
        if b.is_ascii_control() {
            end -= 1;
            continue;
        }
        match b {
            // Sentence punctuation that is unlikely to be part of the URL.
            b'.' | b',' | b';' | b'!' | b'?' => {
                end -= 1;
            }
            // A trailing colon is a typo unless it starts a `:NN` suffix, and
            // a trailing `:` at the very end has no suffix to start.
            b':' => {
                end -= 1;
            }
            // A closing bracket survives only while its opener is inside the
            // URL: `https://a.b/(c)` keeps the `)`, `(https://a.b)` trims it.
            b')' | b']' | b'}' => {
                let open = match b {
                    b')' => b'(',
                    b']' => b'[',
                    _ => b'{',
                };
                let rest = &raw.as_bytes()[..end - 1];
                if rest.contains(&open) {
                    return &raw[start..end];
                }
                end -= 1;
            }
            b'"' | b'\'' => {
                end -= 1;
            }
            _ => return &raw[start..end],
        }
    }
    &raw[start..end]
}
