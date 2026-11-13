//! Result surface of a codemode execution: output items, call records, store
//! writes, and the four-way error classification.
//!
//! `execute` never returns `Err` for a failing script — failure is a result
//! (`CodemodeResult.error`), so the caller always receives the partial output
//! produced before the failure.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Default token budget for the script's text output when `@options` does not
/// set `max_output_tokens`.
pub const DEFAULT_MAX_OUTPUT_TOKENS: usize = 10_000;
/// Hard limit on total text characters a script may emit (`text()`, `image()`
/// base64 data, and `console.*` combined). Exceeding it fails the script.
pub const MAX_OUTPUT_CHARS: usize = 16 * 1024 * 1024;
/// Hard limit on the number of output items. Exceeding it fails the script.
pub const MAX_OUTPUT_ITEMS: usize = 100_000;
/// Global cap on in-flight nested tool calls. Calls issued beyond the cap are
/// rejected immediately with an explanatory error.
pub const MAX_IN_FLIGHT_CALLS: usize = 32;
/// Cap on the call records one result carries (`details.calls`): a script
/// issuing more keeps only the first [`MAX_RECORDED_CALLS`] records in issue
/// order, and `CodemodeResult::total_calls` counts every call it issued.
pub const MAX_RECORDED_CALLS: usize = 100;
/// Estimated tokens of a string: one token per four characters.
pub const CHARS_PER_TOKEN: usize = 4;

/// One entry of the script's output stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputItem {
    /// Text emitted by `text()` or one of the `console.*` methods. Console
    /// items are rendered into the trailing `<console_output>` block.
    Text {
        text: String,
        #[serde(default)]
        console: bool,
    },
    /// An image accepted by `image()`: base64 data plus the detected MIME type.
    Image { data: String, mime_type: String },
}

/// Status of one nested tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallStatus {
    Ok,
    Error,
    /// The call was still in flight when the execution ended; its derived
    /// cancellation token fired and the result never reached the script.
    Cancelled,
}

/// Record of one nested tool call. Calls are recorded in the order the script
/// issued them, regardless of completion order.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallRecord {
    pub name: String,
    pub status: CallStatus,
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Store mutations a successful execution produced. Failed executions never
/// produce writes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoreWrites {
    /// `(key, json)` pairs in write order; `json` is the serialized value text.
    pub set: Vec<(String, String)>,
    /// Keys removed via `store(key, undefined)`.
    pub delete: Vec<String>,
}

/// Four-way classification of why an execution failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodemodeErrorKind {
    /// The script itself failed: syntax error, thrown error, output limits.
    Script,
    /// The `@options` `timeout_ms` deadline fired.
    Timeout,
    /// The caller's cancellation token fired.
    Aborted,
    /// The sandbox or the bridge broke (malformed bridge traffic, a VM thread
    /// that died without reporting). Never caused by the script's own logic.
    Sandbox,
}

/// Why an execution failed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeError {
    pub kind: CodemodeErrorKind,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
}

impl CodemodeError {
    pub fn script(message: impl Into<String>) -> Self {
        Self {
            kind: CodemodeErrorKind::Script,
            message: message.into(),
            stack: None,
        }
    }

    pub fn sandbox(message: impl Into<String>) -> Self {
        Self {
            kind: CodemodeErrorKind::Sandbox,
            message: message.into(),
            stack: None,
        }
    }
}

/// Outcome of one execution.
#[derive(Debug, Clone)]
pub struct CodemodeResult {
    /// Output items in emission order, including whatever was produced before
    /// a failure.
    pub output: Vec<OutputItem>,
    /// The rendered output text (body, then the `<console_output>` block) as it
    /// should reach the model: soft-truncated to `max_output_tokens` with head
    /// and tail kept around an omission marker.
    pub text: String,
    /// The untruncated output text, present only when the soft limit truncated
    /// `text`. The host decides whether to persist it; the crate never writes
    /// files.
    pub full_output: Option<String>,
    /// The script's `return` value as JSON text, `None` when it returned
    /// nothing or the execution failed.
    pub value: Option<String>,
    /// Records of the first [`MAX_RECORDED_CALLS`] nested tool calls, in
    /// issue order; `total_calls` counts every call the script issued.
    pub calls: Vec<CallRecord>,
    /// The number of nested tool calls the script issued — always the true
    /// count, even when the `calls` list was truncated to
    /// [`MAX_RECORDED_CALLS`].
    pub total_calls: usize,
    /// Store mutations; empty unless the execution succeeded.
    pub store_writes: StoreWrites,
    /// Why the execution failed, `None` on success.
    pub error: Option<CodemodeError>,
    /// Wall-clock time of the run, from the moment an execution slot was
    /// granted (a process-wide queueing wait before that is excluded).
    /// Pre-slot failures — a source error, a queue timeout, or an abort while
    /// queued — report the wait instead, since no run happened.
    pub wall_time: Duration,
}

/// Render the output items into the text that reaches the model: body text
/// items joined with newlines, then the `<console_output>` block. Image items
/// do not contribute text (they travel as `OutputItem::Image`).
///
/// When the rendered text exceeds `max_output_tokens` (estimated as chars/4),
/// head and tail are kept around an omission marker and the full text is
/// returned alongside as the `Some` half.
pub fn render_output(items: &[OutputItem], max_output_tokens: usize) -> (String, Option<String>) {
    let mut body: Vec<&str> = Vec::new();
    let mut console: Vec<&str> = Vec::new();
    for item in items {
        match item {
            OutputItem::Text {
                text,
                console: is_console,
            } => {
                if *is_console {
                    console.push(text);
                } else {
                    body.push(text);
                }
            }
            OutputItem::Image { .. } => {}
        }
    }
    let mut full = body.join("\n");
    if !console.is_empty() {
        let block = format!(
            "<console_output>\n{}\n</console_output>",
            console.join("\n")
        );
        if !full.is_empty() {
            full.push('\n');
        }
        full.push_str(&block);
    }

    let budget_chars = max_output_tokens.saturating_mul(CHARS_PER_TOKEN);
    let total = full.chars().count();
    if total <= budget_chars {
        return (full, None);
    }
    let half = budget_chars / 2;
    let chars: Vec<char> = full.chars().collect();
    let omitted = total - 2 * half;
    let marker = format!("\n[... {omitted} characters omitted ...]\n");
    let mut truncated = String::with_capacity(2 * half + marker.len());
    truncated.extend(chars[..half].iter());
    truncated.push_str(&marker);
    truncated.extend(chars[total - half..].iter());
    (truncated, Some(full))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(t: &str) -> OutputItem {
        OutputItem::Text {
            text: t.to_owned(),
            console: false,
        }
    }

    #[test]
    fn render_places_console_block_after_body() {
        let items = vec![
            text("a"),
            OutputItem::Text {
                text: "c1".into(),
                console: true,
            },
            text("b"),
            OutputItem::Text {
                text: "c2".into(),
                console: true,
            },
        ];
        let (rendered, full) = render_output(&items, DEFAULT_MAX_OUTPUT_TOKENS);
        assert_eq!(
            rendered,
            "a\nb\n<console_output>\nc1\nc2\n</console_output>"
        );
        assert_eq!(full, None);
    }

    #[test]
    fn soft_limit_keeps_head_and_tail_and_carries_full_text() {
        let items = vec![text(&"x".repeat(100))];
        let (rendered, full) = render_output(&items, 10); // 40 chars budget
        assert!(rendered.starts_with('x'), "head kept");
        assert!(rendered.ends_with('x'), "tail kept");
        assert_eq!(
            rendered.matches("characters omitted").count(),
            1,
            "one omission marker"
        );
        let total = rendered.chars().count();
        assert!(
            total <= 80,
            "truncated text should stay near the budget, got {total}"
        );
        let full_text = full.expect("full output carried separately");
        assert_eq!(full_text.chars().count(), 100);
    }

    #[test]
    fn under_budget_is_untouched() {
        let items = vec![text("hello")];
        let (rendered, full) = render_output(&items, DEFAULT_MAX_OUTPUT_TOKENS);
        assert_eq!(rendered, "hello");
        assert_eq!(full, None);
    }
}
