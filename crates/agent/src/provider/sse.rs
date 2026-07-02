//! Generic SSE line parsing.
//!
//! Shared across wire providers: reqwest `bytes_stream` → split by line → strip
//! the `data:` prefix → hand each line to the provider's own `serde_json::from_str`.

/// Extract the `data:` payload from a single SSE line. Non-data lines (`event:`, `:` heartbeat, blank) return `None`.
pub fn extract_data_line(line: &str) -> Option<&str> {
    line.strip_prefix("data: ")
        .or_else(|| line.strip_prefix("data:"))
}
