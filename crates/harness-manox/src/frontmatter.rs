//! Shared YAML-frontmatter splitter for markdown definition files.
//!
//! Agent / skill / command definitions all follow the same convention: a leading
//! `---`-fenced YAML block (the metadata) followed by a markdown body. The split
//! logic is identical across the three, so it lives here once — each loader
//! deserializes the frontmatter into its own struct and keeps the body verbatim.

use anyhow::{Context as _, Result};
use serde::de::DeserializeOwned;

/// A parsed definition file: the deserialized frontmatter and the raw body.
pub struct FrontmatterFile<T> {
    pub front: T,
    pub body: String,
}

/// Parse a markdown string into frontmatter + body, deserializing the frontmatter
/// into `T`. A file without a leading `---` fence is treated as body-only and
/// yields an error (callers expect non-empty metadata); callers that want to
/// tolerate body-only files handle the error themselves.
pub fn parse<T: DeserializeOwned>(raw: &str) -> Result<FrontmatterFile<T>> {
    let (front_str, body) = split(raw);
    let front: T = serde_yaml::from_str(front_str).with_context(|| "parsing frontmatter")?;
    Ok(FrontmatterFile { front, body })
}

/// Split a markdown file into `(frontmatter_yaml, body)`. Frontmatter is the
/// content between the first and second `---` lines; everything after the closing
/// fence is the body. A file without a leading `---` line yields an empty
/// frontmatter (caller errors on the empty yaml) and the whole file as body.
pub fn split(raw: &str) -> (&str, String) {
    let raw = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let after_open = match raw
        .strip_prefix("---\n")
        .or_else(|| raw.strip_prefix("---\r\n"))
    {
        Some(s) => s,
        None => return ("", raw.to_string()),
    };
    let mut front_end = None;
    let mut line_start = 0;
    for (i, ch) in after_open.char_indices() {
        if ch != '\n' {
            continue;
        }
        let line = after_open[line_start..i].trim_end_matches('\r');
        if line == "---" {
            front_end = Some(line_start);
            break;
        }
        line_start = i + 1;
    }
    if front_end.is_none() && after_open[line_start..].trim_end_matches('\r') == "---" {
        front_end = Some(line_start);
    }
    match front_end {
        Some(end) => {
            let front = &after_open[..end];
            let body = after_open[end..]
                .strip_prefix("---\n")
                .or_else(|| after_open[end..].strip_prefix("---\r\n"))
                .unwrap_or(&after_open[end..]);
            (front, body.to_string())
        }
        None => (after_open, String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct Meta {
        name: String,
    }

    #[test]
    fn splits_frontmatter_and_body() {
        let raw = "---\nname: x\n---\nbody line\n";
        let (front, body) = split(raw);
        assert_eq!(front, "name: x\n");
        assert_eq!(body, "body line\n");
    }

    #[test]
    fn body_only_yields_empty_frontmatter() {
        let (front, body) = split("no fence\n");
        assert_eq!(front, "");
        assert_eq!(body, "no fence\n");
    }

    #[test]
    fn parse_deserializes_front() {
        let raw = "---\nname: x\n---\nbody\n";
        let f: FrontmatterFile<Meta> = parse(raw).unwrap();
        assert_eq!(f.front.name, "x");
        assert_eq!(f.body, "body\n");
    }
}
