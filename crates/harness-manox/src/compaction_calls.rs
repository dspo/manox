//! The manox harness compaction side calls (request building + streaming
//! summary). Pure compaction-state helpers live in `agent::compact`.

use crate::language_model::{
    AnyLanguageModel, LanguageModelCompletionEvent, LanguageModelRequest,
    LanguageModelRequestMessage,
};
use agent::compact::*;
use agent::language_model::{MessageContent, Role, TokenUsage};
use agent::message::Message;
use agent::thread::model_facing_content;
use anyhow::Result;
use futures::StreamExt as _;
use gpui::AsyncApp;
use tokio_util::sync::CancellationToken;

pub fn build_compaction_request(
    messages: &[Message],
    insertion_ix: usize,
    lang: agent::language::Language,
) -> LanguageModelRequest {
    let bound = insertion_ix.min(messages.len());
    let mut request_messages: Vec<LanguageModelRequestMessage> = Vec::new();

    // ── system prompt ──────────────────────────────────────────────────
    request_messages.push(LanguageModelRequestMessage {
        role: Role::System,
        content: vec![MessageContent::Text(
            agent::prompt::render_static(
                agent::prompt::PromptTemplate::SideCallCompactSystem,
                lang,
            )
            .expect("compact system prompt render"),
        )],
        cache: false,
    });

    // ── incremental or full history ────────────────────────────────────
    // Find the most recent compaction before the insertion point.
    let prev_compaction = latest_compaction_ix(messages, bound);

    if let Some(prev_ix) = prev_compaction {
        // Incremental mode: summarizer sees the previous summary as context
        // plus only the new messages since that compaction.
        let prev_content: String = messages[prev_ix]
            .content
            .iter()
            .filter_map(|c| {
                if let MessageContent::Compaction(text) = c {
                    Some(text.as_str())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let (prev_summary, prev_state) = parse_compaction(&prev_content);
        let state_context = prev_state
            .as_ref()
            .and_then(|state| serde_json::to_string(state).ok())
            .map(|state| format!("\nRuntime state capsule: {state}"))
            .unwrap_or_default();

        // Inject the previous summary as a user-role context block.
        request_messages.push(LanguageModelRequestMessage {
            role: Role::User,
            content: vec![MessageContent::Text(format!(
                "Previous compaction summary (use this as context; summarize ONLY the new messages below):\n\n{prev_summary}{state_context}"
            ))],
            cache: false,
        });

        // Feed only the new messages after the previous compaction.
        for m in &messages[prev_ix + 1..bound] {
            request_messages.push(LanguageModelRequestMessage {
                role: m.role,
                content: m
                    .content
                    .iter()
                    .map(|c| model_facing_content(c, lang))
                    .collect(),
                cache: false,
            });
        }
    } else {
        // Full mode: no prior compaction — summarize the entire history.
        for m in &messages[..bound] {
            request_messages.push(LanguageModelRequestMessage {
                role: m.role,
                content: m
                    .content
                    .iter()
                    .map(|c| model_facing_content(c, lang))
                    .collect(),
                cache: false,
            });
        }
    }

    // ── final instruction ──────────────────────────────────────────────
    request_messages.push(LanguageModelRequestMessage {
        role: Role::User,
        content: vec![MessageContent::Text(
            agent::prompt::render_static(
                agent::prompt::PromptTemplate::SideCallCompactFinalInstruction,
                lang,
            )
            .expect("compact final instruction render"),
        )],
        cache: false,
    });
    let messages = coalesce_same_role(request_messages);
    LanguageModelRequest {
        messages,
        tools: Vec::new(),
        tool_choice: None,
        temperature: Some(0.0),
        thinking_allowed: false,
        reasoning_effort: agent::settings::side_call_effort(
            &crate::settings_ext::side_calls().compaction_policy(),
            crate::language_model::RequestReasoningEffort::Medium,
        ),
        max_output_tokens: agent::settings::side_call_output_cap(
            crate::settings_ext::side_calls().compaction_policy(),
        ),
    }
}

/// Merge runs of consecutive same-role messages by concatenating their content
/// blocks in order. Anthropic's wire rejects adjacent same-role messages;
/// compaction assembles `[retained user...][compaction user][...]` which can
/// produce such runs, so every compaction-shaped request is normalized through
/// this pass before it reaches a provider.
pub fn coalesce_same_role(
    messages: Vec<LanguageModelRequestMessage>,
) -> Vec<LanguageModelRequestMessage> {
    let mut out: Vec<LanguageModelRequestMessage> = Vec::with_capacity(messages.len());
    for m in messages {
        if let Some(last) = out.last_mut()
            && last.role == m.role
        {
            last.content.extend(m.content);
            // A coalesced run is one logical message; keep `cache` as the
            // last segment's flag so a trailing cache anchor survives.
            last.cache = m.cache;
        } else {
            out.push(m);
        }
    }
    out
}

/// Stream a compaction summary from `model` over `request`, draining the
/// response to completion. Returns the accumulated summary text plus the final
/// `TokenUsage` the provider reported (if any) so the caller can attribute the
/// side call's tokens. An empty/whitespace summary is an error: a compaction
/// message with no content is worse than no compaction (it discards history
/// and hands the model nothing). Cancellation yields an error.
pub async fn stream_summary(
    model: &AnyLanguageModel,
    request: LanguageModelRequest,
    cancel: CancellationToken,
    cx: &AsyncApp,
) -> Result<(String, Option<TokenUsage>)> {
    let model = std::sync::Arc::clone(model);
    let call = async move {
        let mut stream = model.stream_completion(request, cx).await?.fuse();
        let mut text = String::new();
        let mut usage: Option<TokenUsage> = None;
        while let Some(event) = stream.next().await {
            let event = event?;
            match event {
                LanguageModelCompletionEvent::Text(delta) => text.push_str(&delta),
                LanguageModelCompletionEvent::UsageUpdate(u) => {
                    // Cumulative for the request; keep the latest (final) snapshot.
                    usage = Some(u);
                }
                LanguageModelCompletionEvent::Stop(_) => break,
                LanguageModelCompletionEvent::Retry { .. }
                | LanguageModelCompletionEvent::ToolUse(_)
                | LanguageModelCompletionEvent::ToolUseJsonParseError { .. }
                | LanguageModelCompletionEvent::Thinking { .. } => {}
            }
        }
        Ok::<_, anyhow::Error>((text, usage))
    };
    let (text, usage) = tokio::select! {
        biased;
        _ = cancel.cancelled() => anyhow::bail!("compaction cancelled"),
        result = call => result?,
    };
    if text.trim().is_empty() {
        anyhow::bail!("compaction produced an empty summary");
    }
    Ok((text, usage))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent::message::Message;

    fn user(id: &str, text: &str) -> Message {
        let mut m = Message::user(text.to_string());
        m.id = id.to_string();
        m
    }

    #[test]
    fn coalesce_merges_consecutive_same_role() {
        use crate::language_model::MessageContent;
        let msgs = vec![
            LanguageModelRequestMessage {
                role: Role::User,
                content: vec![MessageContent::Text("a".into())],
                cache: false,
            },
            LanguageModelRequestMessage {
                role: Role::User,
                content: vec![MessageContent::Text("b".into())],
                cache: true,
            },
            LanguageModelRequestMessage {
                role: Role::Assistant,
                content: vec![MessageContent::Text("c".into())],
                cache: false,
            },
        ];
        let out = coalesce_same_role(msgs);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].role, Role::User);
        assert_eq!(out[0].string_contents(), "ab");
        // Trailing segment's cache flag wins.
        assert!(out[0].cache);
        assert_eq!(out[1].role, Role::Assistant);
    }
    #[test]
    fn build_compaction_request_system_then_history_then_prompt() {
        let msgs = vec![
            user("u1", "what is 1+1"),
            Message::assistant(vec![MessageContent::Text("2".into())]),
        ];
        let req = build_compaction_request(&msgs, 2, agent::language::Language::En);
        // system + 2 history + trailing prompt = 4 after coalesce (no adjacent
        // same-role here: system, user, assistant, user).
        assert_eq!(req.messages.len(), 4);
        assert_eq!(req.messages[0].role, Role::System);
        assert_eq!(req.messages[1].role, Role::User);
        assert_eq!(req.messages[2].role, Role::Assistant);
        assert_eq!(req.messages[3].role, Role::User);
        assert!(req.tools.is_empty());
    }
    #[test]
    fn build_compaction_request_coalesces_trailing_user_run() {
        // History ends on a user message; the trailing "write summary" user
        // turn coalesces into it rather than producing two adjacent user msgs.
        let msgs = vec![
            user("u1", "q"),
            Message::assistant(vec![MessageContent::Text("a".into())]),
            user("u2", "follow-up"),
        ];
        let req = build_compaction_request(&msgs, 3, agent::language::Language::En);
        // system + user + assistant + (user+user coalesced) = 4
        assert_eq!(req.messages.len(), 4);
        assert_eq!(req.messages[3].role, Role::User);
        assert!(req.messages[3].string_contents().contains("follow-up"));
        assert!(
            req.messages[3]
                .string_contents()
                .contains("handoff summary")
        );
    }
    fn compaction(id: &str, summary: &str) -> Message {
        let mut m =
            Message::user_with_content(vec![MessageContent::Compaction(summary.to_string())]);
        m.id = id.to_string();
        m
    }

    fn complete_state() -> agent::compact::CompactionState {
        collect_compaction_state(CompactionStateInput {
            cwd: std::path::Path::new("/repo"),
            covered_message_id: Some("covered-42"),
            worktree_branch: Some("feature/replay"),
            worktree_path: Some("/repo/.worktrees/replay"),
            git_branch: Some("feature/replay"),
            git_status: Some("## feature/replay\n M src/lib.rs".into()),
            plan_steps: Some(vec![PlanStepCapsule {
                title: "verify replay".into(),
                status: "in_progress".into(),
            }]),
            goal: Some("finish issue 299"),
            active_tools: vec!["Read".into(), "Code".into()],
            active_skills: vec!["github".into()],
            background_shells: vec!["shell-7: cargo test".into()],
            artifacts: vec!["docs/report.md".into()],
        })
    }

    #[test]
    fn incremental_compaction_includes_previous_capsule_and_only_delta() {
        let envelope = build_compaction_envelope("previous handoff".into(), complete_state());
        let messages = vec![
            user("old", "must not be replayed"),
            compaction("capsule", &envelope),
            user("delta", "new delta only"),
        ];
        let request =
            build_compaction_request(&messages, messages.len(), agent::language::Language::En);
        let rendered = request
            .messages
            .iter()
            .map(agent::language_model::LanguageModelRequestMessage::string_contents)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("previous handoff"));
        assert!(rendered.contains("covered-42"));
        assert!(rendered.contains("new delta only"));
        assert!(!rendered.contains("must not be replayed"));
    }
}
