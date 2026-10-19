// Canonical selection of a child run's final assistant output.
//
// The fold is incremental over the child's streamed events and independent
// of the run's stop reason: a completed non-empty assistant message is the
// candidate answer, and every streamed text delta extends a bounded
// fallback bucket. A killed run collects whatever it produced — the
// zero-character timeout deliveries are the failure mode this exists to
// end. When the final message list is still reachable (kill paths), it
// refreshes the candidate under the same last-non-empty rule.

use crate::core::types::{AgentEvent, AgentMessage, AssistantMessageEvent, ContentBlock};

/// The fallback bucket's cap: it exists to rescue killed runs, not to
/// mirror the whole transcript.
const STREAMED_MAX_BYTES: usize = 128 * 1024;

#[derive(Default)]
pub struct OutputFold {
    message: Option<String>,
    streamed: String,
}

impl OutputFold {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one streamed child event.
    pub fn observe(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::MessageUpdate {
                assistant_message_event: AssistantMessageEvent::TextDelta { delta, .. },
                ..
            } => self.push_streamed(delta),
            AgentEvent::MessageEnd { message } => self.observe_message(message),
            _ => {}
        }
    }

    /// Refresh the candidate from a final message list (kill paths).
    pub fn observe_messages(&mut self, messages: &[AgentMessage]) {
        for message in messages {
            self.observe_message(message);
        }
    }

    /// The selection: the last non-empty assistant message, else the
    /// accumulated streamed text; `None` when the child produced neither.
    pub fn collect(&self) -> Option<String> {
        if let Some(message) = &self.message {
            return Some(message.clone());
        }
        if self.streamed.is_empty() {
            None
        } else {
            Some(self.streamed.clone())
        }
    }

    fn observe_message(&mut self, message: &AgentMessage) {
        if let AgentMessage::Assistant { content, .. } = message {
            let text: String = content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            if !text.is_empty() {
                self.message = Some(text);
            }
        }
    }

    fn push_streamed(&mut self, delta: &str) {
        if self.streamed.len() < STREAMED_MAX_BYTES {
            self.streamed.push_str(delta);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delta(text: &str) -> AgentEvent {
        AgentEvent::MessageUpdate {
            message: Box::new(assistant("")),
            assistant_message_event: AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: text.into(),
            },
        }
    }

    #[test]
    fn a_completed_message_wins_over_the_streamed_fallback() {
        let mut fold = OutputFold::new();
        fold.observe(&delta("streamed preamble"));
        fold.observe_messages(&[assistant("")]);
        assert_eq!(fold.collect().as_deref(), Some("streamed preamble"));

        fold.observe_messages(&[assistant("the real answer")]);
        assert_eq!(fold.collect().as_deref(), Some("the real answer"));
    }

    #[test]
    fn a_killed_run_falls_back_to_its_accumulated_stream() {
        let mut fold = OutputFold::new();
        fold.observe(&delta("Now the AHP side — "));
        fold.observe(&delta("ext channel definitions."));
        // Only thinking + tool calls followed: no further text anywhere.
        fold.observe_messages(&[]);
        assert_eq!(
            fold.collect().as_deref(),
            Some("Now the AHP side — ext channel definitions.")
        );
    }

    #[test]
    fn a_child_that_produced_nothing_collects_none() {
        let mut fold = OutputFold::new();
        fold.observe(&AgentEvent::AgentStart);
        assert_eq!(fold.collect(), None);
    }

    fn assistant(text: &str) -> AgentMessage {
        serde_json::from_value(serde_json::json!({
            "role": "assistant",
            "content": [{ "type": "text", "text": text }],
            "model": "m",
            "provider": "p",
            "api": "test",
        }))
        .unwrap()
    }
}
