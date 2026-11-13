//! Codemode source format: JavaScript with an optional first-line options
//! directive.
//!
//! ```js
//! // @options: {"max_output_tokens": 2000, "timeout_ms": 30000}
//! const pkg = JSON.parse(await tools.read({ path: "package.json" }));
//! text(pkg.name);
//! ```
//!
//! Only the first line may carry the directive, and only the first line is
//! ever inspected: line-level parsing cannot tell a real directive from an
//! `// @options:`-prefixed line inside the script's own content (a template
//! string, say), so lines beyond the first are left alone — to this parser
//! they are ordinary comments and change nothing. The returned code keeps the
//! original line numbering (the options line becomes an empty line) so error
//! stacks point at the source as the model wrote it.

use crate::types::DEFAULT_MAX_OUTPUT_TOKENS;

pub const OPTIONS_PREFIX: &str = "// @options:";
const SUPPORTED_FIELDS: [&str; 2] = ["max_output_tokens", "timeout_ms"];

/// Options a script may set via the `// @options:` directive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceOptions {
    /// Soft token budget for the script's output text (estimated chars/4).
    pub max_output_tokens: usize,
    /// Hard deadline for the whole execution in milliseconds, including
    /// nested tool calls. `None` = no deadline.
    pub timeout_ms: Option<u64>,
}

impl Default for SourceOptions {
    fn default() -> Self {
        Self {
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
            timeout_ms: None,
        }
    }
}

/// Parsed source: the code with the options line blanked, plus its options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSource {
    pub code: String,
    pub options: SourceOptions,
}

/// A source-level rejection: the script never runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceError {
    pub message: String,
}

impl std::fmt::Display for SourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

fn is_non_negative_int(value: &serde_json::Value) -> bool {
    value.is_u64()
}

/// Split an optional first-line `// @options: {...}` from the script.
pub fn parse_source(input: &str) -> Result<ParsedSource, SourceError> {
    if input.trim().is_empty() {
        return Err(SourceError {
            message: "Expected JavaScript source text (non-empty). Provide JS only, optionally with a first line \
                      `// @options: {\"max_output_tokens\": 1000}`."
                .into(),
        });
    }
    let Some(newline) = input.find('\n') else {
        // Single line: either plain code or an options line with no body.
        let first = input.trim_start();
        if first.starts_with(OPTIONS_PREFIX) {
            return Err(SourceError {
                message:
                    "The @options line must be followed by JavaScript source on subsequent lines"
                        .into(),
            });
        }
        return Ok(ParsedSource {
            code: input.to_owned(),
            options: SourceOptions::default(),
        });
    };
    let first_line = input[..newline].trim_start();
    if !first_line.starts_with(OPTIONS_PREFIX) {
        return Ok(ParsedSource {
            code: input.to_owned(),
            options: SourceOptions::default(),
        });
    }
    // Everything after the first newline; the leading `\n` keeps the options
    // line as an empty line so script line numbers are unchanged.
    let code = input[newline..].to_owned();
    if code.trim().is_empty() {
        return Err(SourceError {
            message: "The @options line must be followed by JavaScript source on subsequent lines"
                .into(),
        });
    }
    let directive = first_line
        .strip_prefix(OPTIONS_PREFIX)
        .expect("checked prefix above")
        .trim();
    let options = parse_options(directive)?;
    Ok(ParsedSource { code, options })
}

fn parse_options(directive: &str) -> Result<SourceOptions, SourceError> {
    let supported = "`max_output_tokens` and `timeout_ms`";
    if directive.is_empty() {
        return Err(SourceError {
            message: format!("@options must be a JSON object with supported fields {supported}"),
        });
    }
    let value: serde_json::Value =
        serde_json::from_str(directive).map_err(|error| SourceError {
            message: format!(
                "@options must be valid JSON with supported fields {supported}: {error}"
            ),
        })?;
    let serde_json::Value::Object(fields) = value else {
        return Err(SourceError {
            message: format!("@options must be a JSON object with supported fields {supported}"),
        });
    };
    let mut options = SourceOptions::default();
    for key in fields.keys() {
        if !SUPPORTED_FIELDS.contains(&key.as_str()) {
            return Err(SourceError {
                message: format!("@options only supports {supported}; got `{key}`"),
            });
        }
    }
    if let Some(value) = fields.get("max_output_tokens") {
        if !is_non_negative_int(value) {
            return Err(SourceError {
                message: "@options field `max_output_tokens` must be a non-negative integer".into(),
            });
        }
        options.max_output_tokens = value.as_u64().expect("checked u64 above") as usize;
    }
    if let Some(value) = fields.get("timeout_ms") {
        if !is_non_negative_int(value) || value.as_u64() == Some(0) {
            return Err(SourceError {
                message: "@options field `timeout_ms` must be a positive integer".into(),
            });
        }
        options.timeout_ms = Some(value.as_u64().expect("checked u64 above"));
    }
    Ok(options)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_source_passes_through() {
        let parsed = parse_source("const a = 1;\ntext(a);\n").expect("parses");
        assert_eq!(parsed.code, "const a = 1;\ntext(a);\n");
        assert_eq!(parsed.options, SourceOptions::default());
    }

    #[test]
    fn options_line_is_blanked_and_line_numbers_align() {
        let source = "// @options: {\"max_output_tokens\": 500, \"timeout_ms\": 1000}\nconst a = 1;\ntext(a);\n";
        let parsed = parse_source(source).expect("parses");
        assert_eq!(parsed.options.max_output_tokens, 500);
        assert_eq!(parsed.options.timeout_ms, Some(1000));
        assert_eq!(parsed.code, "\nconst a = 1;\ntext(a);\n");
        // Line 2 of the parsed code is line 2 of the source.
        assert_eq!(parsed.code.lines().nth(1), Some("const a = 1;"));
    }

    #[test]
    fn empty_input_is_rejected() {
        assert!(parse_source("").is_err());
        assert!(parse_source("   \n  ").is_err());
    }

    #[test]
    fn options_only_is_rejected() {
        let error = parse_source("// @options: {\"timeout_ms\": 5}").expect_err("no code body");
        assert!(
            error
                .message
                .contains("must be followed by JavaScript source")
        );
        let error =
            parse_source("// @options: {\"timeout_ms\": 5}\n   \n").expect_err("no code body");
        assert!(
            error
                .message
                .contains("must be followed by JavaScript source")
        );
    }

    #[test]
    fn options_prefix_on_a_later_line_is_an_ordinary_comment() {
        // Line-level parsing cannot distinguish a directive from an
        // options-prefixed line inside the script's own content (a template
        // string, say), so only the first line is inspected: a later
        // options-prefixed line changes nothing and never fails the parse.
        let source = "const a = 1;\n// @options: {\"timeout_ms\": 5}\ntext(a);\n";
        let parsed = parse_source(source).expect("parses");
        assert_eq!(parsed.options, SourceOptions::default());
        assert_eq!(parsed.code, source, "the line stays verbatim in the code");
    }

    #[test]
    fn invalid_options_are_rejected() {
        assert!(
            parse_source("// @options:\nconst a = 1;\n").is_err(),
            "empty directive"
        );
        assert!(
            parse_source("// @options: {broken\nconst a = 1;\n").is_err(),
            "invalid JSON"
        );
        assert!(
            parse_source("// @options: []\nconst a = 1;\n").is_err(),
            "not an object"
        );
        assert!(
            parse_source("// @options: {\"unknown\": 1}\nconst a = 1;\n").is_err(),
            "unknown field"
        );
        assert!(
            parse_source("// @options: {\"max_output_tokens\": -1}\nconst a = 1;\n").is_err(),
            "negative budget"
        );
        assert!(
            parse_source("// @options: {\"timeout_ms\": 0}\nconst a = 1;\n").is_err(),
            "zero timeout"
        );
        assert!(
            parse_source("// @options: {\"timeout_ms\": \"x\"}\nconst a = 1;\n").is_err(),
            "string timeout"
        );
    }
}
