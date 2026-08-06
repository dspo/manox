//! Content hash for snapshot tags.
//!
//! The tag is a 4-hex fingerprint of the whole file's normalized text: any read
//! of byte-identical content mints the same tag, and a follow-up edit anchored
//! at any line validates whenever the live file still hashes to it. Trailing
//! whitespace and CRLF endings are normalized away before hashing so display
//! trimming and line-ending differences never invalidate a tag.

use xxhash_rust::xxh32::xxh32 as xxhash32;

/// Tag length in hex characters.
pub const TAG_LEN: usize = 4;

/// Normalize text before hashing: trim trailing `[ \t\r]` from every line
/// (including the final line) so CRLF endings and display-trimmed lines do not
/// invalidate a tag.
pub fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.split('\n') {
        let trimmed = line.trim_end_matches([' ', '\t', '\r']);
        out.push_str(trimmed);
        out.push('\n');
    }
    // The split above always produces a trailing empty segment for text ending
    // in `\n`; remove the single trailing newline we added to match the
    // leaving the body without an extra trailing newline).
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Compute the 4-uppercase-hex content tag from raw file text.
pub fn compute_tag(text: &str) -> String {
    let normalized = normalize(text);
    let low16 = xxhash32(normalized.as_bytes(), 0) & 0xffff;
    format!("{:0>4X}", low16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_across_trailing_whitespace() {
        let a = "fn main() {\n    println!(\"hi\");\n}\n";
        let b = "fn main() {   \n    println!(\"hi\");\t\n}\n";
        assert_eq!(compute_tag(a), compute_tag(b));
    }

    #[test]
    fn stable_across_crlf() {
        let lf = "a\nb\nc\n";
        let crlf = "a\r\nb\r\nc\r\n";
        assert_eq!(compute_tag(lf), compute_tag(crlf));
    }

    #[test]
    fn differs_on_content_change() {
        let a = "fn main() {}\n";
        let b = "fn other() {}\n";
        assert_ne!(compute_tag(a), compute_tag(b));
    }

    #[test]
    fn tag_is_four_upper_hex() {
        let tag = compute_tag("anything");
        assert_eq!(tag.len(), TAG_LEN);
        assert!(tag.chars().all(|c| matches!(c, '0'..='9' | 'A'..='F')));
    }
}
