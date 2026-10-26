//! Request-side pruning of historical thinking blocks.
//!
//! A subagent can run many turns in one child session, and every request
//! replays the whole transcript. Reasoning blocks from turns that are already
//! closed have no protocol role in the new turn — only the current turn's
//! thinking must survive (Anthropic requires it for the tool loop) — so
//! dropping them before the request saves tokens without changing what the
//! model can see.
//!
//! The journal keeps the original blocks: this rewrite is request-only.

use std::sync::Arc;

use crate::harness::{HookContext, HookHandler};
use crate::types::{AgentMessage, ContentBlock};

/// Drop thinking blocks from assistant messages that precede the last user
/// message, then remove assistant messages left empty by the rewrite.
///
/// The last `User` message is the current turn's boundary (the same heuristic
/// compaction uses): everything after it is the live tool loop and keeps its
/// thinking verbatim. With no user message the transcript is a first request
/// and is left untouched. Returns whether any block was removed.
pub fn strip_historical_thinking(messages: &mut Vec<AgentMessage>) -> bool {
    let Some(boundary) = messages
        .iter()
        .rposition(|message| matches!(message, AgentMessage::User { .. }))
    else {
        return false;
    };
    let current_turn = messages.split_off(boundary);
    let mut stripped = false;
    messages.retain_mut(|message| {
        let AgentMessage::Assistant { content, .. } = message else {
            return true;
        };
        let before = content.len();
        content.retain(|block| !matches!(block, ContentBlock::Thinking { .. }));
        stripped |= content.len() != before;
        !content.is_empty()
    });
    messages.extend(current_turn);
    stripped
}

/// The `BeforeProviderRequest` handler for a child that does not replay
/// historical thinking.
///
/// A run that actually removed a block marks the context so provider
/// translation can tell a stripped turn from a response that never reasoned:
/// only the former loses its unpaired `fc_` item ids.
pub fn strip_historical_thinking_hook() -> HookHandler {
    Arc::new(|mut ctx: HookContext| {
        if let Some(agent_context) = ctx.agent_context.as_mut()
            && strip_historical_thinking(&mut agent_context.messages)
        {
            agent_context.metadata.insert(
                crate::types::THINKING_STRIPPED_METADATA_KEY.to_string(),
                serde_json::Value::Bool(true),
            );
        }
        ctx
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::StopReason;
    use serde_json::json;

    fn user(text: &str) -> AgentMessage {
        AgentMessage::user(text)
    }

    fn assistant(content: Vec<ContentBlock>) -> AgentMessage {
        AgentMessage::Assistant {
            content,
            model: "test-model".into(),
            provider: "test-provider".into(),
            api: "anthropic".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            stop_reason: Some(StopReason::Stop),
            raw_stop_reason: None,
            usage: Box::default(),
            error_message: None,
            timestamp: chrono::Utc::now(),
        }
    }

    fn thinking(text: &str, redacted: bool) -> ContentBlock {
        ContentBlock::Thinking {
            thinking: text.into(),
            signature: Some("sig".into()),
            redacted: redacted.then_some(true),
        }
    }

    fn text(text: &str) -> ContentBlock {
        ContentBlock::Text {
            text: text.into(),
            signature: None,
        }
    }

    fn thinking_count(messages: &[AgentMessage]) -> usize {
        messages
            .iter()
            .map(|message| match message {
                AgentMessage::Assistant { content, .. } => content
                    .iter()
                    .filter(|block| matches!(block, ContentBlock::Thinking { .. }))
                    .count(),
                _ => 0,
            })
            .sum()
    }

    #[test]
    fn historical_thinking_is_dropped_and_the_current_turn_is_kept() {
        let mut messages = vec![
            user("first turn"),
            assistant(vec![thinking("old", false), text("old answer")]),
            user("current turn"),
            assistant(vec![thinking("live", false), text("live answer")]),
        ];
        strip_historical_thinking(&mut messages);
        assert_eq!(messages.len(), 4);
        assert_eq!(
            thinking_count(&messages),
            1,
            "only the current turn keeps it"
        );
        assert!(matches!(
            &messages[3],
            AgentMessage::Assistant { content, .. }
                if content.iter().any(|b| matches!(b, ContentBlock::Thinking { thinking, .. } if thinking == "live"))
        ));
    }

    #[test]
    fn redacted_thinking_is_dropped_too() {
        let mut messages = vec![
            user("first"),
            assistant(vec![thinking("", true), text("answer")]),
            user("second"),
        ];
        strip_historical_thinking(&mut messages);
        assert_eq!(thinking_count(&messages), 0);
        assert_eq!(messages.len(), 3, "the assistant keeps its text");
    }

    #[test]
    fn an_assistant_stripped_to_nothing_is_removed() {
        let mut messages = vec![
            user("first"),
            assistant(vec![thinking("only reasoning", false)]),
            user("second"),
        ];
        strip_historical_thinking(&mut messages);
        assert_eq!(messages.len(), 2, "the empty assistant row is gone");
        assert!(matches!(&messages[0], AgentMessage::User { .. }));
        assert!(matches!(&messages[1], AgentMessage::User { .. }));
    }

    #[test]
    fn a_tool_use_keeps_its_message_even_without_thinking() {
        let mut messages = vec![
            user("first"),
            assistant(vec![
                thinking("old", false),
                ContentBlock::ToolUse {
                    id: "call-1".into(),
                    name: "Read".into(),
                    input: json!({}),
                    thought_signature: None,
                },
            ]),
            AgentMessage::ToolResult {
                tool_call_id: "call-1".into(),
                tool_name: "Read".into(),
                content: vec![text("ok")],
                is_error: false,
                details: None,
                usage: None,
                added_tool_names: None,
                timestamp: chrono::Utc::now(),
            },
            user("second"),
        ];
        strip_historical_thinking(&mut messages);
        assert_eq!(messages.len(), 4);
        assert_eq!(thinking_count(&messages), 0);
        assert!(
            matches!(&messages[1], AgentMessage::Assistant { content, .. } if content.len() == 1)
        );
    }

    #[test]
    fn without_a_user_boundary_nothing_changes() {
        let mut messages = vec![
            AgentMessage::Custom {
                custom_type: "note".into(),
                content: vec![text("context")],
                display: false,
                details: None,
                timestamp: chrono::Utc::now(),
            },
            assistant(vec![thinking("keep", false), text("answer")]),
        ];
        let before = messages.len();
        strip_historical_thinking(&mut messages);
        assert_eq!(messages.len(), before);
        assert_eq!(thinking_count(&messages), 1);
    }

    fn context(messages: Vec<AgentMessage>) -> crate::types::AgentContext {
        crate::types::AgentContext {
            system_prompt: String::new(),
            messages,
            tools: std::sync::Arc::from(Vec::new()),
            model: crate::types::Model {
                provider: "anthropic".into(),
                id: "test-model".into(),
                api: "anthropic".into(),
                context_window: 200_000,
                max_tokens: 8_192,
                thinking: crate::types::ThinkingKind::Enabled,
                metadata: Default::default(),
            },
            thinking_level: None,
            cache_retention: Default::default(),
            session_id: None,
            metadata: Default::default(),
            stream_options: Default::default(),
        }
    }

    #[test]
    fn the_hook_marks_a_request_it_rewrote() {
        let hook = strip_historical_thinking_hook();
        let ctx = HookContext::new(crate::harness::HookPoint::BeforeProviderRequest).with_context(
            context(vec![
                user("first"),
                assistant(vec![thinking("old", false), text("old")]),
                user("current"),
            ]),
        );
        let rewritten = hook(ctx).agent_context.expect("context flows through");
        assert!(
            rewritten
                .metadata
                .contains_key(crate::types::THINKING_STRIPPED_METADATA_KEY),
            "provider translation must know the history was rewritten"
        );
        assert_eq!(thinking_count(&rewritten.messages), 0);

        let untouched = hook(
            HookContext::new(crate::harness::HookPoint::BeforeProviderRequest)
                .with_context(context(vec![user("first request")])),
        )
        .agent_context
        .expect("context flows through");
        assert!(
            !untouched
                .metadata
                .contains_key(crate::types::THINKING_STRIPPED_METADATA_KEY),
            "a request with nothing to strip keeps the unmarked path"
        );
    }
}
