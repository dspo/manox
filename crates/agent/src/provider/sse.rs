//! Generic SSE line parsing and shared stream utilities.
//!
//! `extract_data_line` is shared across wire providers: reqwest `bytes_stream`
//! → split by line → strip the `data:` prefix → hand each line to the
//! provider's own `serde_json::from_str`.
//!
//! `fix_streamed_json` repairs a streamed (possibly truncated) JSON fragment
//! into a complete JSON value. The Anthropic, Responses, and Chat Completions
//! wires all stream tool-input arguments as concatenated JSON string chunks;
//! the last chunk is rarely a complete document, so each wire's mapper feeds
//! the partial buffer through this helper to recover the current value.

use anyhow::{Result, anyhow};

/// Extract the `data:` payload from a single SSE line. Non-data lines (`event:`, `:` heartbeat, blank) return `None`.
pub fn extract_data_line(line: &str) -> Option<&str> {
    line.strip_prefix("data: ")
        .or_else(|| line.strip_prefix("data:"))
}

/// Repair a streamed (possibly incomplete) JSON fragment into a complete JSON value that serde accepts.
///
/// Tracks each open delimiter by kind so it can be closed with its matching
/// closer; a scalar depth counter would close every `[` with `}`, corrupting
/// any tool input that contains an array literal.
pub fn fix_streamed_json(s: &str) -> Result<serde_json::Value> {
    serde_json::from_str::<serde_json::Value>(s).or_else(|_| {
        let mut fixed = String::from(s);
        let mut stack: Vec<char> = Vec::new();
        let mut in_string = false;
        let mut escaped = false;
        for ch in fixed.chars() {
            if in_string {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '"' {
                    in_string = false;
                }
            } else if ch == '"' {
                in_string = true;
            } else if ch == '{' || ch == '[' {
                stack.push(ch);
            } else if ch == '}' || ch == ']' {
                stack.pop();
            }
        }
        if in_string {
            fixed.push('"');
        }
        while let Some(open) = stack.pop() {
            fixed.push(if open == '{' { '}' } else { ']' });
        }
        serde_json::from_str::<serde_json::Value>(&fixed)
            .map_err(|e| anyhow!("fix_streamed_json failed: {e}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closes_arrays_and_objects() {
        // Truncated object containing an array literal: a scalar depth counter
        // would close the `[` with `}`, yielding invalid JSON.
        let fixed = fix_streamed_json(r#"{"files": ["a.rs"#).expect("repair array");
        assert_eq!(fixed["files"][0], "a.rs");

        let fixed = fix_streamed_json(r#"{"a": {"b": 1"#).expect("repair nested");
        assert_eq!(fixed["a"]["b"], 1);

        let fixed = fix_streamed_json(r#"{"note": "hello"#).expect("repair string");
        assert_eq!(fixed["note"], "hello");

        let fixed = fix_streamed_json(r#"{"ok": true}"#).expect("passthrough");
        assert!(fixed["ok"].is_boolean());
    }
}
