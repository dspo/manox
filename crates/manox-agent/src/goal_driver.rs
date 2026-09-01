//! Goal round driver: the idle gate that queues one automatic continuation
//! round, and the settle logic that admits it (round accounting + token
//! accounting) when the run actually consumed it.
//!
//! Delivery uses the pi harness's `follow_up` queue: the round message is an
//! `AgentMessage::Custom` with `display: false`, so the model sees it as a
//! user-role message on the wire (`convert_to_llm` projects Custom onto
//! User), while the UI mirror drops it (`harness_messages_to_messages`
//! ignores non-displayed Custom). Admission — the durable fact that the round
//! message entered the transcript — is checked at settle by matching the
//! reserved identity against the harness transcript, so a queued round that
//! never ran is never charged.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use manox_harness::coding_agent::AgentSession;
use manox_harness::harness::HarnessHandle;
use manox_harness::types::{AgentMessage, ContentBlock};

use crate::goal::{GoalActor, GoalBlockReason, GoalStatus, ThreadGoal};
use crate::goal_tools::GoalBridge;

/// Identity of the one reserved (queued) continuation round.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalRoundIdentity {
    pub goal_id: String,
    pub revision: u64,
    pub round: u64,
}

/// Goal-budget tokens of one pi usage report — excludes provider cache reads
/// by definition (`budget_tokens` parity).
fn budget_from_pi_usage(usage: &manox_harness::types::Usage) -> u64 {
    usage.input_tokens + usage.cache_creation_input_tokens + usage.output_tokens
}

/// Model-visible continuation prompt for one goal round. The objective is
/// JSON-quoted so multiline or tag-like text stays data, never markup.
pub fn render_goal_round_prompt(goal: &ThreadGoal, round: u64) -> String {
    let max = goal
        .max_rounds
        .map(|max| max.to_string())
        .unwrap_or_else(|| "∞".to_string());
    let budget = goal
        .remaining_tokens()
        .map(|remaining| format!("\nRemaining token budget: {remaining}"))
        .unwrap_or_default();
    format!(
        "<goal_round>\nObjective: {objective}\nRound: {round}/{max}{budget}\n\n\
         Continue working toward the objective in this same session. Treat the current workspace, \
         tool results, and durable session state as authoritative; inspect them instead of assuming \
         earlier narration is still current. Make concrete progress and verify the result. Before \
         claiming completion, gather evidence that the whole objective is achieved, read the \
         current goal, and mark it complete. If work remains, leave the goal active for the next \
         round. Follow the goal tool policy before reporting a blocker.\n</goal_round>",
        objective = serde_json::to_string(&goal.objective).unwrap_or_default(),
    )
}

/// Model-visible closing instruction injected after an autonomous round
/// reports `complete`, so the model addresses the user once before the turn
/// ends instead of stopping silently.
pub fn render_goal_complete_wrapup(objective: &str) -> String {
    format!(
        "<goal_complete>\nObjective: {objective}\nThe goal is marked complete and this autonomous \
         run is ending. Write the closing message to the user now: state the outcome, summarize \
         what was done and how it was verified, and point to the concrete results (files, commits, \
         or other artifacts). Report only what earlier rounds and tool results in this session \
         actually establish; when a detail is not in the session, say so instead of inventing it. \
         Note anything the user should review or do next. Address the user directly. Do not call \
         any more tools in this run; further work waits for the user's next instruction.\n\
         </goal_complete>",
        objective = serde_json::to_string(objective).unwrap_or_default(),
    )
}

/// Model-visible closing instruction injected after an autonomous round
/// reports `blocked`.
pub fn render_goal_blocked_wrapup(objective: &str, reason: &str) -> String {
    format!(
        "<goal_blocked>\nObjective: {objective}\nBlocked: {reason}\nThe goal is marked blocked and \
         this autonomous run is ending. Write the closing message to the user now: state what has \
         been completed so far, describe the concrete blocking condition and what you tried, and \
         say exactly what you need from the user to continue. Report only what earlier rounds and \
         tool results in this session actually establish; when a detail is not in the session, say \
         so instead of inventing it. Address the user directly. Do not call any more tools in this \
         run; further work waits for the user's next instruction.\n</goal_blocked>",
        objective = serde_json::to_string(objective).unwrap_or_default(),
        reason = serde_json::to_string(reason).unwrap_or_default(),
    )
}

/// The idle gate. Returns true when a continuation round was queued; the
/// caller then runs `session.continue_()` and re-checks after settle.
///
/// Short-circuits (in order): a round already reserved, no active armed goal,
/// round cap exhausted (auto-block with `round-limit`), or competing input
/// queued (yield, DSH parity).
pub async fn maybe_queue_goal_round(
    session: &AgentSession,
    bridge: &Arc<GoalBridge>,
    continuation_reserved: &AtomicBool,
    continuation_round: &Mutex<Option<GoalRoundIdentity>>,
    handle: &HarnessHandle,
) -> bool {
    if continuation_reserved.load(std::sync::atomic::Ordering::Relaxed) {
        return false;
    }
    let Some(goal) = bridge.snapshot() else {
        return false;
    };
    if goal.status != GoalStatus::Active || !bridge.armed() {
        return false;
    }
    if goal
        .max_rounds
        .is_some_and(|max| goal.rounds_started >= max)
    {
        let _ = bridge.set_status(
            GoalStatus::Blocked,
            Some(GoalBlockReason {
                code: "round-limit".into(),
                message: format!(
                    "Goal reached its configured limit of {} rounds.",
                    goal.max_rounds.unwrap_or_default()
                ),
            }),
            GoalActor::System,
        );
        return false;
    }
    // Competing input yields to the human; a later settle re-checks.
    if !session.steering_messages().is_empty() || session.has_next_turn() {
        return false;
    }
    let round = goal.rounds_started + 1;
    let identity = GoalRoundIdentity {
        goal_id: goal.goal_id.clone(),
        revision: goal.revision,
        round,
    };
    let text = render_goal_round_prompt(&goal, round);
    let message = AgentMessage::Custom {
        custom_type: "goal_round".to_string(),
        content: vec![ContentBlock::Text {
            text,
            signature: None,
        }],
        display: false,
        details: Some(serde_json::json!({
            "round": round,
            "goal_id": goal.goal_id,
            "revision": goal.revision,
        })),
        timestamp: chrono::Utc::now(),
    };
    // After compaction the transcript may end on a summary user message;
    // `continue_` only drains queued messages when the tail is an assistant
    // message, so route through the steering queue in that case. The steer
    // queue is shared with user steers — admission at settle is still
    // unambiguous because the round's `{round, goal_id, revision}` triple
    // in `details` is matched against the reserved identity, never against
    // queue position or ordering.
    let tail_is_assistant = matches!(
        session.harness_messages().last(),
        Some(AgentMessage::Assistant { .. })
    );
    if tail_is_assistant {
        handle.follow_up(message);
    } else {
        handle.steer(message);
    }
    continuation_reserved.store(true, std::sync::atomic::Ordering::Relaxed);
    *continuation_round.lock().unwrap() = Some(identity);
    bridge.mark_goal_round_active(true);
    true
}

/// Settle one goal round after its run: admit it (round + token accounting)
/// only when the reserved message actually entered the transcript, pause
/// durably on user abort, and disarm (never auto-retry) on any failure.
pub async fn settle_goal_round(
    session: &AgentSession,
    bridge: &Arc<GoalBridge>,
    continuation_reserved: &AtomicBool,
    continuation_round: &Mutex<Option<GoalRoundIdentity>>,
    run_result: &anyhow::Result<Vec<AgentMessage>>,
    abort_requested: bool,
) -> anyhow::Result<()> {
    let Some(identity) = continuation_round.lock().unwrap().take() else {
        return Ok(());
    };
    let admitted = transcript_contains_round(session, &identity);
    if admitted {
        let tokens_delta = run_result
            .as_ref()
            .map(|messages| {
                messages
                    .iter()
                    .filter_map(|message| match message {
                        AgentMessage::Assistant { usage, .. } => Some(budget_from_pi_usage(usage)),
                        AgentMessage::ToolResult { usage, .. } => {
                            usage.as_ref().map(budget_from_pi_usage)
                        }
                        _ => None,
                    })
                    .sum()
            })
            .unwrap_or(0);
        if bridge
            .account_round(
                identity.round,
                identity.revision,
                identity.goal_id.clone(),
                tokens_delta,
            )
            .is_err()
        {
            // Persistence failure: fail closed, never auto-retry.
            bridge.disarm();
        }
        if abort_requested {
            let _ = bridge.set_status(
                GoalStatus::Paused,
                Some(GoalBlockReason {
                    code: "round-interrupted".into(),
                    message: "goal round was interrupted".into(),
                }),
                GoalActor::System,
            );
        }
    } else {
        // The round message never entered the transcript: do not charge it
        // and never auto-retry — a human resume re-arms.
        bridge.disarm();
    }
    continuation_reserved.store(false, std::sync::atomic::Ordering::Relaxed);
    bridge.mark_goal_round_active(false);
    Ok(())
}

/// Whether the reserved round message is present in the harness transcript.
fn transcript_contains_round(session: &AgentSession, identity: &GoalRoundIdentity) -> bool {
    session.harness_messages().iter().any(|message| {
        if let AgentMessage::Custom {
            custom_type,
            details,
            ..
        } = message
        {
            *custom_type == "goal_round"
                && details.as_ref().is_some_and(|details| {
                    details.get("round").and_then(|v| v.as_u64()) == Some(identity.round)
                        && details.get("goal_id").and_then(|v| v.as_str())
                            == Some(identity.goal_id.as_str())
                        && details.get("revision").and_then(|v| v.as_u64())
                            == Some(identity.revision)
                })
        } else {
            false
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_prompt_json_quotes_objective_and_lists_round() {
        let mut goal =
            ThreadGoal::new("t".into(), "multi\nline <tag>".into(), None, Some(3)).unwrap();
        goal.rounds_started = 1;
        let text = render_goal_round_prompt(&goal, 2);
        assert!(text.contains("\"multi\\nline <tag>\""));
        assert!(text.contains("Round: 2/3"));
        assert!(text.contains("<goal_round>"));
        assert!(!text.contains("Remaining token budget"));
    }

    #[test]
    fn round_prompt_lists_remaining_budget_when_set() {
        let goal = ThreadGoal::new("t".into(), "x".into(), Some(100), None).unwrap();
        let text = render_goal_round_prompt(&goal, 1);
        assert!(text.contains("Remaining token budget: 100"));
        assert!(text.contains("Round: 1/∞"));
    }

    #[test]
    fn wrapups_carry_grounding_and_no_tool_directive() {
        let complete = render_goal_complete_wrapup("ship it");
        assert!(complete.contains("<goal_complete>"));
        assert!(complete.contains("\"ship it\""));
        assert!(complete.contains("Do not call any more tools in this run"));
        let blocked = render_goal_blocked_wrapup("ship it", "missing key");
        assert!(blocked.contains("<goal_blocked>"));
        assert!(blocked.contains("\"missing key\""));
    }
}
