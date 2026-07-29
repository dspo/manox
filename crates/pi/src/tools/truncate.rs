// Output truncation for tool results.
//
// Tool outputs that exceed the configured limits are truncated to avoid
// overwhelming the context window. Truncation preserves the beginning and
// end of the output, with a clear marker indicating how much was omitted.

/// Default maximum bytes for a tool result.
pub const DEFAULT_MAX_BYTES: usize = 128 * 1024; // 128 KiB

/// Default maximum lines for a tool result.
pub const DEFAULT_MAX_LINES: usize = 2000;

/// Truncation configuration.
#[derive(Debug, Clone)]
pub struct TruncateConfig {
    pub max_bytes: usize,
    pub max_lines: usize,
}

impl Default for TruncateConfig {
    fn default() -> Self {
        TruncateConfig {
            max_bytes: DEFAULT_MAX_BYTES,
            max_lines: DEFAULT_MAX_LINES,
        }
    }
}

/// The result of truncating output.
#[derive(Debug, Clone)]
pub struct TruncatedOutput {
    /// The (possibly truncated) content.
    pub content: String,
    /// Whether truncation was applied.
    pub was_truncated: bool,
    /// Original byte count before truncation.
    pub original_bytes: usize,
    /// Original line count before truncation.
    pub original_lines: usize,
}

/// Truncate output to fit within the configured limits.
///
/// Truncation is applied in this order:
/// 1. If line count exceeds `max_lines`, keep the first half and last half.
/// 2. If byte count exceeds `max_bytes`, truncate to `max_bytes` and append
///    a truncation notice.
pub fn truncate(output: &str, config: &TruncateConfig) -> TruncatedOutput {
    let original_bytes = output.len();
    let original_lines = output.lines().count();

    let mut truncated = output.to_string();
    let mut was_truncated = false;

    // Truncate by lines first.
    if original_lines > config.max_lines {
        let lines: Vec<&str> = output.lines().collect();
        let half = config.max_lines / 2;
        let head: Vec<&str> = lines.iter().take(half).copied().collect();
        let tail: Vec<&str> = lines
            .iter()
            .rev()
            .take(half)
            .copied()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();

        let skipped = original_lines - config.max_lines;
        truncated = format!(
            "{}\n\n... [{} lines truncated] ...\n\n{}",
            head.join("\n"),
            skipped,
            tail.join("\n")
        );
        was_truncated = true;
    }

    // Truncate by bytes.
    if truncated.len() > config.max_bytes {
        let head_len = config.max_bytes * 3 / 4;
        let tail_len = config.max_bytes - head_len;

        let head = if let Some(idx) = truncated.char_indices().nth(head_len) {
            &truncated[..idx.0]
        } else {
            &truncated
        };

        let tail_start = truncated.len().saturating_sub(tail_len);
        let tail = if let Some(idx) = truncated.char_indices().nth(tail_start) {
            &truncated[idx.0..]
        } else {
            ""
        };

        let skipped_bytes = original_bytes.saturating_sub(
            head.len() + tail.len(),
        );

        truncated = format!(
            "{}\n\n... [{} bytes truncated] ...\n\n{}",
            head, skipped_bytes, tail
        );
        was_truncated = true;
    }

    TruncatedOutput {
        content: truncated,
        was_truncated,
        original_bytes,
        original_lines,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_truncation_needed() {
        let config = TruncateConfig::default();
        let result = truncate("hello world", &config);
        assert!(!result.was_truncated);
        assert_eq!(result.content, "hello world");
    }

    #[test]
    fn test_truncate_by_lines() {
        let config = TruncateConfig {
            max_lines: 10,
            max_bytes: usize::MAX,
        };
        let lines: Vec<String> = (0..100).map(|i| format!("line {i}")).collect();
        let input = lines.join("\n");

        let result = truncate(&input, &config);
        assert!(result.was_truncated);
        assert!(result.content.contains("lines truncated"));
        // Should have roughly 10 lines + truncation marker.
        let output_lines = result.content.lines().count();
        assert!(output_lines < 20, "got {output_lines} lines, expected < 20");
    }

    #[test]
    fn test_truncate_by_bytes() {
        let config = TruncateConfig {
            max_lines: usize::MAX,
            max_bytes: 100,
        };
        let input = "x".repeat(1000);

        let result = truncate(&input, &config);
        assert!(result.was_truncated);
        assert!(result.content.contains("bytes truncated"));
        assert!(result.content.len() <= 200, "got {} bytes", result.content.len());
    }
}