//! Journal → AHP translation.
//!
//! The durable journal stays the source of truth (architecture doc §C, L3/L4):
//! every observable state change is a journal entry, and the AHP action stream
//! is a *projection* of that journal. This module owns the mapping in one
//! direction only:
//!
//! ```text
//! JournalWireEntry ──target_of──▶ channel ──actions──▶ StateAction ──▶ Host::publish
//! ```
//!
//! [`target_of`] is an exhaustive match over the entry vocabulary: a kernel
//! entry added without deciding where it lands on the wire is a compile error,
//! which is how the "no journal kind is wire-less" rule (v2 §C.2) survives the
//! protocol change. W2 adds the action construction on top of this
//! classification; the classification itself is what fixes the channel
//! boundary, so it lands with the host skeleton.

use manox_journal::JournalWireEvent;

/// The channel family a journal entry publishes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// `ahp-chat:/<id>` — transcript rows, streaming, tool calls, approvals.
    Chat,
    /// `ahp-session:/<id>` — title, working directories, config, catalog.
    Session,
    /// `x-manox-plan:/<chat-id>` — plan mode, plan document, plan review.
    Plan,
    /// `x-manox-work:/<session-id>` — goal, background work, sub-agents, suites.
    Work,
    /// `x-manox-metrics:/<chat-id>` — aggregated conversation metrics.
    Metrics,
}

impl Target {
    /// The channel URI this target resolves to, given the owning chat/session.
    pub fn uri(self, chat_id: &str, session_id: &str) -> String {
        match self {
            Self::Chat => crate::channels::chat::uri(chat_id),
            Self::Session => crate::channels::session::uri(session_id),
            Self::Plan => format!("{}{chat_id}", crate::ext::channels::PLAN),
            Self::Work => format!("{}{session_id}", crate::ext::channels::WORK),
            Self::Metrics => format!("{}{chat_id}", crate::ext::channels::METRICS),
        }
    }
}

/// Which channel an entry lands on.
pub fn target_of(event: &JournalWireEvent) -> Target {
    use JournalWireEvent as E;
    match event {
        // ── transcript ───────────────────────────────────────────────
        E::Message { .. } | E::UiNote { .. } | E::Custom { .. } | E::CustomMessage { .. } => {
            Target::Chat
        }
        // ── lifecycle ────────────────────────────────────────────────
        E::TurnStart
        | E::TurnFinish { .. }
        | E::Stop { .. }
        | E::Retry { .. }
        | E::Error { .. } => Target::Chat,
        // ── streaming / tool activity ────────────────────────────────
        E::AgentTextDelta { .. }
        | E::AgentThinkingDelta { .. }
        | E::ToolCall { .. }
        | E::ToolResult { .. }
        | E::ToolOutputChunk { .. }
        | E::SubagentChild { .. } => Target::Chat,
        E::SubagentProgress { .. } => Target::Work,
        // ── session state ────────────────────────────────────────────
        E::ModelChange { .. }
        | E::CwdChange { .. }
        | E::ProjectChange { .. }
        | E::PermissionModeChange { .. }
        | E::ReasoningEffortChange { .. }
        | E::Title { .. }
        | E::PinnedArchived { .. }
        | E::Label { .. }
        | E::SessionInfo { .. }
        | E::Leaf { .. } => Target::Session,
        // ── plan ─────────────────────────────────────────────────────
        E::PlanModeChange { .. }
        | E::PlanModeRequest { .. }
        | E::PlanUpdate { .. }
        | E::PlanReview { .. } => Target::Plan,
        // ── extension work surface ───────────────────────────────────
        E::Goal { .. }
        | E::BrowserSuites { .. }
        | E::BackgroundTask { .. }
        | E::ActiveToolsChange { .. } => Target::Work,
        // ── approvals / questions ride the chat's tool-call + input state ──
        E::Approval { .. } | E::Question { .. } => Target::Chat,
        // ── compaction and branch summaries surface in the transcript ──
        E::Compaction { .. } | E::CompactionStarted { .. } | E::BranchSummary { .. } => {
            Target::Chat
        }
        // ── metrics ──────────────────────────────────────────────────
        E::Metrics { .. } => Target::Metrics,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_entry_kind_has_a_channel() {
        // Compile-time totality is the real gate (the match above is exhaustive
        // by construction); this asserts the classification on samples so a
        // silent reshuffle of the mapping shows up as a failure.
        let samples = [
            (
                JournalWireEvent::AgentTextDelta { s: "x".into() },
                Target::Chat,
            ),
            (JournalWireEvent::TurnStart, Target::Chat),
            (
                JournalWireEvent::ToolCall {
                    call_id: "t".into(),
                    name: "bash".into(),
                    title: "run".into(),
                    status: "running".into(),
                    input: serde_json::json!({}),
                },
                Target::Chat,
            ),
            (
                JournalWireEvent::Approval {
                    kind: "request".into(),
                    auth_id: "a-1".into(),
                    tool_name: Some("bash".into()),
                    tool_call_id: Some("t".into()),
                    verdict: None,
                    reason: None,
                },
                Target::Chat,
            ),
            (
                JournalWireEvent::PlanModeChange { enabled: true },
                Target::Plan,
            ),
            (
                JournalWireEvent::Title { title: "t".into() },
                Target::Session,
            ),
            (JournalWireEvent::Goal { goal: None }, Target::Work),
            (
                JournalWireEvent::Metrics {
                    kind: "main_call".into(),
                    data: serde_json::json!({}),
                },
                Target::Metrics,
            ),
        ];
        for (event, expected) in samples {
            assert_eq!(target_of(&event), expected, "for {event:?}");
        }
    }

    #[test]
    fn targets_resolve_to_declared_channels() {
        assert_eq!(Target::Chat.uri("c-1", "s-1"), "ahp-chat:/c-1");
        assert_eq!(Target::Session.uri("c-1", "s-1"), "ahp-session:/s-1");
        assert_eq!(Target::Plan.uri("c-1", "s-1"), "x-manox-plan:/c-1");
        assert_eq!(Target::Work.uri("c-1", "s-1"), "x-manox-work:/s-1");
        assert_eq!(Target::Metrics.uri("c-1", "s-1"), "x-manox-metrics:/c-1");
    }
}
