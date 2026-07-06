//! Hashline editing: line-anchored patches validated by content-hash tags.
//!
//! `read_file` mints a 4-hex tag from the file's normalized text and records a
//! snapshot; `edit_file` parses a patch of `SWAP`/`DEL`/`INS` ops anchored on
//! the ORIGINAL line numbers, validates the tag still matches the live file,
//! and applies the ops back-to-front. On a stale tag, a 3-way merge replays the
//! resolved changes onto the current content by content-anchoring the snapshot
//! ranges. The model never has to reproduce the original text verbatim — only
//! the line numbers and a fresh tag.
//!
//! Global state is a `OnceLock<Mutex<SnapshotStore>>` mirroring the
//! `runtime` / `thread_store` singleton pattern. Tools access it via
//! [`global`], which panics if [`init`] was not called at app startup.

pub mod apply;
pub mod block;
pub mod hash;
pub mod parser;
pub mod recovery;
pub mod snapshot;

#[cfg(test)]
mod integration_tests;

use std::sync::{Mutex, OnceLock};

pub use apply::{ApplyError, ApplyResult, apply};
pub use block::BlockError;
pub use hash::compute_tag;
pub use parser::{FilePatch, InsPos, Op, ParseError, parse_patch};
pub use recovery::{RecoverError, try_recover, try_recover_with_snapshot};
pub use snapshot::{Snapshot, SnapshotStore};

static GLOBAL: OnceLock<Mutex<SnapshotStore>> = OnceLock::new();

/// Initialize the global snapshot store. Call once at app startup from
/// [`crate::init`]. Idempotent: a second call is a no-op.
pub fn init() {
    let _ = GLOBAL.set(Mutex::new(SnapshotStore::new()));
}

/// The global snapshot store. Panics if [`init`] was not called.
pub fn global() -> &'static Mutex<SnapshotStore> {
    GLOBAL
        .get()
        .expect("hashline snapshot store not initialized; call agent::init first")
}

/// Normalize raw file bytes for hashing and line-number display: strip a leading
/// BOM and convert CRLF to LF. Trailing-whitespace normalization for the hash
/// happens inside [`hash::compute_tag`].
pub fn normalize_to_lf(raw: &str) -> String {
    raw.strip_prefix('\u{feff}')
        .unwrap_or(raw)
        .replace("\r\n", "\n")
        .replace('\r', "\n")
}

/// Detect whether `raw` used CRLF line endings, so an edit can restore them on
/// write instead of silently flattening to LF.
pub fn detect_crlf(raw: &str) -> bool {
    raw.contains("\r\n")
}

/// Detect whether `raw` began with a UTF-8 BOM.
pub fn has_bom(raw: &str) -> bool {
    raw.starts_with('\u{feff}')
}

/// Format a file for `read_file` output: a `[path#TAG]` header followed by
/// `N:TEXT` numbered lines. The caller is responsible for recording the snapshot
/// before formatting so the tag is stable.
pub fn format_numbered(path: &str, text: &str, tag: &str) -> String {
    let mut out = String::with_capacity(text.len() + path.len() + 16);
    out.push('[');
    out.push_str(path);
    out.push('#');
    out.push_str(tag);
    out.push(']');
    out.push('\n');
    for (i, line) in text.lines().enumerate() {
        use std::fmt::Write as _;
        let _ = write!(out, "{}:{}", i + 1, line);
        out.push('\n');
    }
    // Trim the trailing newline so the output matches the conventional shape.
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_bom_and_crlf() {
        let raw = "\u{feff}a\r\nb\r\n";
        assert_eq!(normalize_to_lf(raw), "a\nb\n");
    }

    #[test]
    fn detect_crlf_and_bom() {
        assert!(detect_crlf("a\r\nb\r\n"));
        assert!(!detect_crlf("a\nb\n"));
        assert!(has_bom("\u{feff}a"));
        assert!(!has_bom("a"));
    }

    #[test]
    fn format_numbered_shapes_header_and_lines() {
        let out = format_numbered("a.rs", "fn main() {\n}", "1A2B");
        assert_eq!(out, "[a.rs#1A2B]\n1:fn main() {\n2:}");
    }
}
