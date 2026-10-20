//! HTML→Markdown presentation for a fetched page: the conversion the model
//! reads, plus the two guards the raw converter needs around it.
//!
//! - **Non-content elements never reach the model**: `script`, `style`,
//!   `noscript`, `template`, `iframe`, `object`, `embed`, and any element hidden
//!   by `hidden`, `aria-hidden="true"`, or an inline style that hides it. Page
//!   boilerplate and commented-out scaffolding are noise in a fetched document.
//! - **Conversion depth is bounded** by [`MAX_CONVERSION_DEPTH`]. The converter
//!   walks the element tree recursively, so unclosed tags nest one stack frame
//!   each; a 100k-character body of `<div>` reaches ~20,000 frames and overflows
//!   the worker stack, which aborts the whole process rather than failing the
//!   call (measured here: fine at 2,000 levels, abort at 3,000 on a 2 MiB
//!   stack). A body past the ceiling is omitted, never converted.
//!
//! The scan that enforces the ceiling over-counts malformed input on purpose:
//! a body it cannot model lexically must not be the one that reaches the tree
//! walk. Balanced tags inside an HTML comment still pop, so a commented-out
//! block of ordinary markup does not refuse its page.
use htmd::options::{BulletListMarker, Options};
use htmd::{HtmlToMarkdown, HtmlToMarkdownBuilder};
use markup5ever_rcdom::{Handle, NodeData};

/// Nesting depth above which a body is omitted instead of converted.
pub const MAX_CONVERSION_DEPTH: usize = 512;

/// Elements that never contribute model-facing text.
const NON_CONTENT_ELEMENTS: &[&str] = &[
    "script", "style", "noscript", "template", "iframe", "object", "embed",
];

/// Elements that never take a closing tag, so they do not grow the lexical stack.
const VOID_ELEMENTS: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

/// Elements whose contents the HTML parser reads as text until the matching end tag.
const RAW_TEXT_ELEMENTS: &[&str] = &["script", "style", "noscript"];

/// Convert a decoded HTML body to model-facing Markdown, or `None` when the
/// body is refused (too deeply nested to convert safely, or unparseable).
pub fn to_markdown(html: &str) -> Option<String> {
    if exceeds_conversion_depth(html) {
        return None;
    }
    let converter = converter();
    let tree = converter.html_to_tree(html).ok()?;
    strip_non_content(&tree);
    Some(converter.tree_to_markdown(&tree))
}

fn converter() -> HtmlToMarkdown {
    HtmlToMarkdownBuilder::new()
        .options(Options {
            bullet_list_marker: BulletListMarker::Dash,
            ..Options::default()
        })
        .build()
}

/// Drop every non-content element, including any element hidden by `hidden`,
/// `aria-hidden="true"`, or an inline style that hides it.
fn strip_non_content(node: &Handle) {
    node.children
        .borrow_mut()
        .retain(|child| !is_non_content(child));
    let children: Vec<Handle> = node.children.borrow().clone();
    for child in children {
        strip_non_content(&child);
    }
}

fn is_non_content(node: &Handle) -> bool {
    let NodeData::Element { name, attrs, .. } = &node.data else {
        return false;
    };
    let tag = name.local.as_ref();
    if NON_CONTENT_ELEMENTS.contains(&tag) {
        return true;
    }
    let attrs = attrs.borrow();
    if attrs
        .iter()
        .any(|attr| attr.name.local.as_ref() == "hidden")
    {
        return true;
    }
    if attrs.iter().any(|attr| {
        attr.name.local.as_ref() == "aria-hidden" && attr.value.trim().eq_ignore_ascii_case("true")
    }) {
        return true;
    }
    attrs
        .iter()
        .any(|attr| attr.name.local.as_ref() == "style" && hides(attr.value.as_ref()))
}

/// Whether an inline `style` declaration hides the element.
fn hides(style: &str) -> bool {
    style.split(';').any(|declaration| {
        let Some((property, value)) = declaration.split_once(':') else {
            return false;
        };
        let value = value.trim().to_ascii_lowercase();
        let value = value.strip_suffix("!important").unwrap_or(&value).trim();
        match property.trim().to_ascii_lowercase().as_str() {
            "display" => value == "none",
            "visibility" => value == "hidden" || value == "collapse",
            _ => false,
        }
    })
}

fn exceeds_conversion_depth(html: &str) -> bool {
    let lower = html.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut open: Vec<&str> = Vec::new();
    let mut offset = 0usize;
    let mut in_comment = false;
    while offset < bytes.len() {
        let Some(relative) = lower[offset..].find('<') else {
            break;
        };
        let start = offset + relative;
        if in_comment
            && let Some(end) = lower[offset..].find("-->").map(|e| offset + e)
            && end < start
        {
            in_comment = false;
            offset = end + 3;
            continue;
        }
        if !in_comment && lower[start..].starts_with("<!--") {
            in_comment = true;
            offset = start + 4;
            continue;
        }
        let mut cursor = start + 1;
        let closing = bytes.get(cursor) == Some(&b'/');
        if closing {
            cursor += 1;
        }
        let name_start = cursor;
        while bytes
            .get(cursor)
            .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'-')
        {
            cursor += 1;
        }
        if cursor == name_start || !bytes.get(name_start).is_some_and(u8::is_ascii_alphabetic) {
            offset = start + 1;
            continue;
        }
        let name = &lower[name_start..cursor];
        let mut quote: Option<u8> = None;
        while cursor < bytes.len() {
            let byte = bytes[cursor];
            cursor += 1;
            match quote {
                Some(open_quote) if byte == open_quote => quote = None,
                Some(_) => {}
                None if byte == b'"' || byte == b'\'' => quote = Some(byte),
                None if byte == b'>' => break,
                None => {}
            }
        }
        if cursor == 0 || bytes.get(cursor - 1) != Some(&b'>') {
            break;
        }
        if closing {
            if open.last() == Some(&name) {
                open.pop();
            }
        } else {
            let mut last = cursor as isize - 2;
            while last >= 0 && bytes[last as usize].is_ascii_whitespace() {
                last -= 1;
            }
            let self_closing = last >= 0 && bytes[last as usize] == b'/';
            if !VOID_ELEMENTS.contains(&name) && !self_closing {
                open.push(name);
                if open.len() > MAX_CONVERSION_DEPTH {
                    return true;
                }
                if !in_comment && RAW_TEXT_ELEMENTS.contains(&name) {
                    match find_raw_text_end(&lower, name, cursor) {
                        Some(end) => {
                            offset = end;
                            continue;
                        }
                        None => break,
                    }
                }
            }
        }
        offset = cursor;
    }
    false
}

/// Find the matching raw-text end tag without interpreting markup-like body text.
fn find_raw_text_end(lower: &str, name: &str, from: usize) -> Option<usize> {
    let prefix = format!("</{name}");
    let bytes = lower.as_bytes();
    let mut candidate = lower[from..].find(&prefix).map(|c| from + c)?;
    loop {
        let after = bytes.get(candidate + prefix.len()).copied();
        if after.is_none_or(|b| b == b'>' || b == b'/' || b.is_ascii_whitespace()) {
            return Some(candidate);
        }
        candidate = lower[candidate + prefix.len()..]
            .find(&prefix)
            .map(|c| candidate + prefix.len() + c)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_structure_to_markdown() {
        let html = "<html><body><h1>Title</h1><p>Body <b>bold</b> <code>f()</code></p>\
                    <ul><li>one</li><li>two</li></ul>\
                    <pre><code>fn main() {}\n</code></pre></body></html>";
        let out = to_markdown(html).expect("converts");
        assert!(out.contains("# Title"), "{out}");
        assert!(out.contains("**bold**"), "{out}");
        assert!(out.contains("`f()`"), "{out}");
        assert!(out.contains("-   one"), "{out}");
        assert!(out.contains("```"), "{out}");
        assert!(!out.contains("<h1>"), "{out}");
    }

    #[test]
    fn converts_tables_to_markdown() {
        let html = "<table><thead><tr><th>a</th><th>b</th></tr></thead>\
                    <tbody><tr><td>1</td><td>2</td></tr></tbody></table>";
        let out = to_markdown(html).expect("converts");
        assert!(out.contains("| a | b |"), "{out}");
        assert!(out.contains("| 1 | 2 |"), "{out}");
    }

    #[test]
    fn drops_non_content_and_hidden_elements() {
        let html = "<body><style>.x{color:red}</style><script>var s=1;</script>\
                    <noscript>noscriptbody</noscript><iframe src=\"x\"></iframe>\
                    <div style=\"display:none\">HIDDENSTYLE</div>\
                    <div hidden>HIDDENATTR</div>\
                    <div aria-hidden=\"true\">HIDDENARIA</div>\
                    <div style=\"visibility: collapse !important\">HIDDENVIS</div>\
                    <p>visible</p></body>";
        let out = to_markdown(html).expect("converts");
        assert!(out.contains("visible"), "{out}");
        for leaked in [
            "color:red",
            "var s=1",
            "noscriptbody",
            "HIDDENSTYLE",
            "HIDDENATTR",
            "HIDDENARIA",
            "HIDDENVIS",
        ] {
            assert!(!out.contains(leaked), "{leaked} leaked: {out}");
        }
    }

    #[test]
    fn depth_ceiling_refuses_deeply_nested_markup() {
        let shallow = "<div><div><p>text</p></div></div>";
        assert!(to_markdown(shallow).is_some());
        let deep = "<div>".repeat(MAX_CONVERSION_DEPTH + 1);
        assert!(to_markdown(&deep).is_none());
        let at_ceiling = format!("{}text", "<div>".repeat(MAX_CONVERSION_DEPTH));
        assert!(to_markdown(&at_ceiling).is_some());
    }

    #[test]
    fn depth_scan_ignores_raw_text_and_void_elements() {
        let script = format!(
            "<script>{}</script><p>text</p>",
            "<div>".repeat(MAX_CONVERSION_DEPTH * 4)
        );
        assert!(to_markdown(&script).is_some(), "raw text must not count");
        let voids = "<br>".repeat(MAX_CONVERSION_DEPTH * 4);
        assert!(
            to_markdown(&voids).is_some(),
            "void elements must not count"
        );
        let quoted = format!(
            "<p title=\"{}\">text</p>",
            ">".repeat(MAX_CONVERSION_DEPTH * 4)
        );
        assert!(
            to_markdown(&quoted).is_some(),
            "quoted '>' must not end a tag"
        );
    }

    #[test]
    fn depth_scan_counts_unbalanced_tags_in_comments() {
        // A commented-out *balanced* block closes what it opened, so it stays
        // convertible...
        let balanced = format!(
            "<!-- {} --><p>text</p>",
            "<div>x</div>".repeat(MAX_CONVERSION_DEPTH * 2)
        );
        assert!(to_markdown(&balanced).is_some(), "balanced comments pop");
        // ...while an unbalanced body is refused rather than converted: the
        // scan over-counts malformed input on purpose, so no lexical trick can
        // hide real nesting from the ceiling.
        let unbalanced = format!(
            "<!-- {} --><p>text</p>",
            "<div>".repeat(MAX_CONVERSION_DEPTH * 2)
        );
        assert!(
            to_markdown(&unbalanced).is_none(),
            "unbalanced comments must over-count"
        );
    }

    #[test]
    fn hostile_nesting_is_refused_before_conversion() {
        let hostile: String = "<div>".repeat(20_000);
        assert!(to_markdown(&hostile).is_none());
    }
}
