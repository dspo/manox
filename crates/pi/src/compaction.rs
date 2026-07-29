// Compaction — context window management through summarization.
//
// When the conversation grows too large for the model's context window,
// compaction generates a structured summary of the oldest messages, keeping
// only the most recent turns intact.

use serde::{Deserialize, Serialize};

/// Compaction settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionSettings {
    /// Whether compaction is enabled.
    pub enabled: bool,
    /// Tokens to reserve for the response.
    pub reserve_tokens: usize,
    /// Tokens to keep from the recent conversation tail.
    pub keep_recent_tokens: usize,
}

impl Default for CompactionSettings {
    fn default() -> Self {
        CompactionSettings {
            enabled: true,
            reserve_tokens: 16384,
            keep_recent_tokens: 20000,
        }
    }
}

/// Whether compaction should be triggered.
pub fn should_compact(
    context_tokens: usize,
    context_window: usize,
    settings: &CompactionSettings,
) -> bool {
    settings.enabled && context_tokens > context_window.saturating_sub(settings.reserve_tokens)
}

/// Estimate the token count for a message using the character/4 heuristic.
pub fn estimate_tokens(text: &str) -> usize {
    text.chars().count() / 4
}

/// Estimate token count for a list of messages.
pub fn estimate_context_tokens(messages: &[crate::types::AgentMessage]) -> usize {
    messages
        .iter()
        .map(|m| {
            let serialized = serde_json::to_string(m).unwrap_or_default();
            estimate_tokens(&serialized)
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_compact() {
        let settings = CompactionSettings::default();
        // 100_000 tokens, 200_000 context window, 16_384 reserve
        // 100_000 > 200_000 - 16_384 = 183_616 → false
        assert!(!should_compact(100_000, 200_000, &settings));
        // 190_000 > 183_616 → true
        assert!(should_compact(190_000, 200_000, &settings));
    }

    #[test]
    fn test_estimate_tokens() {
        assert_eq!(estimate_tokens("hello"), 1);
        assert_eq!(estimate_tokens(""), 0);
    }
}