//! Spec tests for link detection — the matching boundaries are the contract.

use std::ops::Range;
use std::path::PathBuf;

use crate::*;

fn urls(text: &str) -> Vec<String> {
    detect_urls(text).into_iter().map(|s| s.href).collect()
}

fn paths(text: &str, opts: &PathOptions) -> Vec<String> {
    detect_paths(text, opts)
        .into_iter()
        .map(|s| s.href)
        .collect()
}

#[test]
fn multi_protocol_schemes() {
    assert_eq!(urls("go https://a.b/c"), vec!["https://a.b/c"]);
    assert_eq!(urls("go http://a.b"), vec!["http://a.b"]);
    assert_eq!(
        urls("mail me mailto:dev@example.com now"),
        vec!["mailto:dev@example.com"]
    );
    assert_eq!(
        urls("clone git://github.com/dspo/manox"),
        vec!["git://github.com/dspo/manox"]
    );
    assert_eq!(urls("ssh:git@host:repo"), vec!["ssh:git@host:repo"]);
    assert_eq!(urls("ipfs ipfs:QmHash"), vec!["ipfs:QmHash"]);
    assert_eq!(urls("gemini gemini://capsule"), vec!["gemini://capsule"]);
    assert_eq!(urls("ftp ftp://host/file"), vec!["ftp://host/file"]);
    assert_eq!(urls("file file:///etc/hosts"), vec!["file:///etc/hosts"]);
}

#[test]
fn https_scan_prefers_longest_scheme() {
    // The scanner resumes after the match, so the `http:` inside `https://`
    // must not split the link.
    assert_eq!(urls("https://a.b/x"), vec!["https://a.b/x"]);
}

#[test]
fn empty_host_does_not_match() {
    assert!(urls("https://").is_empty());
    assert!(urls("mailto:").is_empty());
    assert!(urls("go https:// then x").is_empty());
}

#[test]
fn trailing_punctuation_is_trimmed() {
    assert_eq!(urls("see https://a.b/c."), vec!["https://a.b/c"]);
    assert_eq!(urls("see https://a.b/c,"), vec!["https://a.b/c"]);
    assert_eq!(urls("see https://a.b/c;"), vec!["https://a.b/c"]);
    assert_eq!(urls("see https://a.b/c!"), vec!["https://a.b/c"]);
    assert_eq!(urls("see https://a.b/c?"), vec!["https://a.b/c"]);
    assert_eq!(urls("see https://a.b/c:"), vec!["https://a.b/c"]);
}

#[test]
fn balanced_closing_brackets_kept_unbalanced_trimmed() {
    // The closing paren has no opener inside the URL → trimmed.
    assert_eq!(urls("(see https://a.b/c)"), vec!["https://a.b/c"]);
    // Balanced parens are part of the URL.
    assert_eq!(
        urls("https://en.wikipedia.org/wiki/Foo_(bar)"),
        vec!["https://en.wikipedia.org/wiki/Foo_(bar)"]
    );
    assert_eq!(urls("[x] https://a.b/c] y"), vec!["https://a.b/c"]);
}

#[test]
fn terminator_chars_stop_the_scan() {
    assert_eq!(urls("see <https://a.b/c> ok"), vec!["https://a.b/c"]);
    assert_eq!(urls("see https://a.b/c\"quoted"), vec!["https://a.b/c"]);
    assert_eq!(urls("https://a.b/c`x`"), vec!["https://a.b/c"]);
}

#[test]
fn control_characters_do_not_enter_the_link() {
    assert_eq!(urls("https://a.b/c\x1b[31m"), vec!["https://a.b/c"]);
    assert_eq!(urls("https://a.b/c\tnext"), vec!["https://a.b/c"]);
}

#[test]
fn ranges_are_byte_accurate() {
    // Non-ASCII prefix: the span range must index into the original bytes.
    let text = "看 https://a.b/c 这里";
    let spans = detect_urls(text);
    assert_eq!(spans.len(), 1);
    let span = &spans[0];
    assert_eq!(&text[span.range.clone()], "https://a.b/c");
    assert_eq!(span.kind, UrlKind::Url);
}

#[test]
fn multiple_urls_detected() {
    assert_eq!(
        urls("a https://a.b c http://c.d e"),
        vec!["https://a.b", "http://c.d"]
    );
}

#[test]
fn path_with_extension_detected() {
    let opts = default_path_options();
    assert_eq!(
        paths("see crates/manox-agent/src/thread.rs for it", &opts),
        vec!["crates/manox-agent/src/thread.rs"]
    );
    assert_eq!(paths("open /tmp/foo.txt", &opts), vec!["/tmp/foo.txt"]);
}

#[test]
fn path_line_col_anchor_swallowed() {
    let opts = default_path_options();
    assert_eq!(paths("see src/main.rs:42", &opts), vec!["src/main.rs:42"]);
    assert_eq!(
        paths("see src/main.rs:42-100", &opts),
        vec!["src/main.rs:42-100"]
    );
    assert_eq!(
        paths("see src/main.rs:42:10", &opts),
        vec!["src/main.rs:42:10"]
    );
    // A bare filename with an anchor is still not a path (no `/`).
    assert!(paths("see main.rs:42", &opts).is_empty());
}

#[test]
fn extensionless_path_requires_cwd_existence() {
    let opts = PathOptions {
        cwd: None,
        ..default_path_options()
    };
    assert!(paths("run target/debug/manox", &opts).is_empty());
    // An absolute existing directory is a path even without an extension.
    let opts = PathOptions {
        cwd: Some(PathBuf::from("/")),
        ..default_path_options()
    };
    assert!(!paths("look /tmp", &opts).is_empty());
}

#[test]
fn relative_path_under_cwd_exists() {
    let dir = std::env::temp_dir().join(format!("hyperlinks-cwd-{}", std::process::id()));
    let _ = std::fs::create_dir_all(dir.join("a/b"));
    let opts = PathOptions {
        cwd: Some(dir.clone()),
        ..default_path_options()
    };
    assert_eq!(paths("see a/b for it", &opts), vec!["a/b"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn known_extension_whitelist_overrides_heuristic() {
    let opts = PathOptions {
        known_extensions: vec!["verylongextension"],
        ..default_path_options()
    };
    // 16-char extension fails the 1..=10 heuristic, matches the whitelist.
    assert_eq!(
        paths("see a/b.verylongextension", &opts),
        vec!["a/b.verylongextension"]
    );
}

#[test]
fn bare_filename_without_directory_is_not_a_path() {
    let opts = default_path_options();
    assert!(paths("see README.md", &opts).is_empty());
}

#[test]
fn urls_win_over_paths_when_consolidating() {
    // The URL "https://a.b/c" is also path-like ("c" is an extension); only
    // the URL span survives consolidation.
    let opts = default_path_options();
    let mut spans = detect_urls("go https://a.b/c");
    spans.extend(detect_paths("go https://a.b/c", &opts));
    let consolidated = consolidate(spans);
    assert_eq!(consolidated.len(), 1);
    assert_eq!(consolidated[0].kind, UrlKind::Url);
}

#[test]
fn consolidate_sorts_and_dedupes() {
    let mk = |start, end, kind| OverlaySpan {
        href: format!("{start}-{end}"),
        range: Range { start, end },
        kind,
    };
    let spans = consolidate(vec![
        mk(10, 20, UrlKind::Path),
        mk(0, 5, UrlKind::Url),
        mk(12, 18, UrlKind::Path), // overlaps the first → dropped
        mk(3, 8, UrlKind::Path),   // overlaps [0,5) → dropped
    ]);
    assert_eq!(spans.len(), 2);
    assert_eq!(spans[0].range, 0..5);
    assert_eq!(spans[1].range, 10..20);
}

#[test]
fn is_covered_overlap_semantics() {
    let covered = vec![Range { start: 5, end: 10 }];
    assert!(is_covered(6..9, &covered));
    assert!(is_covered(4..6, &covered));
    assert!(!is_covered(10..12, &covered));
    assert!(!is_covered(0..5, &covered));
}

#[test]
fn trim_url_keeps_port_colon() {
    assert_eq!(trim_url("https://a.b:8080"), "https://a.b:8080");
    assert_eq!(trim_url("https://a.b/c:42"), "https://a.b/c:42");
}
