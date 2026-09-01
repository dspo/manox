//! Persistent Goal domain model and lifecycle runtime.
//!
//! A Goal is the durable autonomy contract for one main thread. Completion is
//! reported explicitly through the Goal tools; there is deliberately no
//! per-turn evaluator in this module.
//!
//! The durable state is event-sourced: every mutation appends a full
//! post-mutation snapshot (`Created`/`Updated`) or a tombstone (`Cleared`) to
//! the thread's `thread_events` stream, and a `Round` event records one
//! admitted continuation round with its token accounting delta. The current
//! goal is always the strict fold of that stream (`fold_goal_events`); the
//! fold is fail-loud — a corrupt, malformed, or out-of-order event is an
//! error, never a silent fallback.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

/// Maximum number of Unicode scalar values accepted for a Goal objective.
pub const MAX_OBJECTIVE_CHARS: usize = 4_000;

/// Minimum number of admitted goal rounds before the model may report a
/// blocker through `UpdateGoal` — the anti-bail gate, matching the DSH
/// tool-goal `blockedAfterConsecutiveRounds` default of 3.
pub const BLOCKED_MIN_GOAL_ROUNDS: u64 = 3;

/// The single supported goal event version; the fold rejects anything else.
pub const GOAL_EVENT_VERSION: u32 = 1;

/// Durable lifecycle state of a Goal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    Active,
    Paused,
    Blocked,
    BudgetLimited,
    Complete,
}

impl GoalStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Blocked => "blocked",
            Self::BudgetLimited => "budget_limited",
            Self::Complete => "complete",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "active" => Self::Active,
            "paused" => Self::Paused,
            "blocked" => Self::Blocked,
            "budget_limited" => Self::BudgetLimited,
            "complete" => Self::Complete,
            _ => bail!("unknown Goal status: {value}"),
        })
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Blocked | Self::BudgetLimited | Self::Complete)
    }

    pub fn can_transition_to(self, next: Self) -> bool {
        self == next
            || matches!(
                (self, next),
                (
                    Self::Active,
                    Self::Paused | Self::Blocked | Self::BudgetLimited | Self::Complete
                ) | (
                    Self::Paused | Self::Blocked | Self::BudgetLimited,
                    Self::Active
                ) | (Self::BudgetLimited, Self::Blocked | Self::Complete)
            )
    }
}

/// The acting side of a Goal mutation, recorded in every event for the audit
/// trail. Enforcement is by tool-input design and call sites, not this tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalActor {
    User,
    Model,
    System,
}

/// Machine-routable and human-readable explanation attached to a blocked or
/// budget-limited Goal. `code` is a stable lower-kebab-case classification
/// chosen by the policy that wrote it; `message` is shown to humans and models.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalBlockReason {
    pub code: String,
    pub message: String,
}

/// The mutation carried by a `goal_updated` event. Token accounting has no
/// operation of its own — token deltas ride `Round` events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalOperation {
    Edit,
    Pause,
    Resume,
    Complete,
    Block,
}

/// One durable goal event, serialized into the `data` column of a
/// `thread_events` row. `kind` is internally tagged on `event` so the payload
/// self-describes against the `event_type` column.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalEvent {
    pub version: u32,
    #[serde(flatten)]
    pub kind: GoalEventKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum GoalEventKind {
    Created {
        actor: GoalActor,
        goal: ThreadGoal,
        created_at: i64,
    },
    Updated {
        actor: GoalActor,
        operation: GoalOperation,
        goal: ThreadGoal,
        turn_id: Option<String>,
        created_at: i64,
    },
    Round {
        goal_id: String,
        revision: u64,
        round: u64,
        turn_id: String,
        tokens_delta: u64,
        admitted_at: i64,
    },
    Cleared {
        actor: GoalActor,
        goal_id: String,
        revision: u64,
        cleared_at: i64,
    },
}

impl GoalEvent {
    /// The stable wire type stored in the `thread_events.event_type` column.
    pub fn event_type(&self) -> &'static str {
        match &self.kind {
            GoalEventKind::Created { .. } => "goal_created",
            GoalEventKind::Updated { .. } => "goal_updated",
            GoalEventKind::Round { .. } => "goal_round",
            GoalEventKind::Cleared { .. } => "goal_cleared",
        }
    }
}

/// The one current Goal owned by a main thread. Revision is the compare-and-set
/// identity: every non-round mutation increments it by one, so a stale writer
/// can never overwrite a newer snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadGoal {
    pub thread_id: String,
    pub goal_id: String,
    pub objective: String,
    pub status: GoalStatus,
    pub token_budget: Option<u64>,
    pub tokens_used: u64,
    pub max_rounds: Option<u64>,
    pub rounds_started: u64,
    pub revision: u64,
    pub blocked_reason: Option<GoalBlockReason>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl ThreadGoal {
    /// The initial Active snapshot of a fresh Goal: revision 1, no rounds,
    /// no accounting.
    pub fn new(
        thread_id: String,
        objective: String,
        token_budget: Option<u64>,
        max_rounds: Option<u64>,
    ) -> Result<Self> {
        let objective = validate_objective(objective)?;
        validate_budget(token_budget)?;
        validate_max_rounds(max_rounds)?;
        let now = chrono::Utc::now().timestamp();
        Ok(Self {
            thread_id,
            goal_id: uuid::Uuid::new_v4().to_string(),
            objective,
            status: GoalStatus::Active,
            token_budget,
            tokens_used: 0,
            max_rounds,
            rounds_started: 0,
            revision: 1,
            blocked_reason: None,
            created_at: now,
            updated_at: now,
        })
    }

    pub fn remaining_tokens(&self) -> Option<u64> {
        self.token_budget
            .map(|budget| budget.saturating_sub(self.tokens_used))
    }

    /// A Goal may be resumed only while the budget and the round cap both have
    /// headroom, and only when it is not complete.
    pub fn can_resume(&self) -> Result<()> {
        if let Some(budget) = self.token_budget
            && self.tokens_used >= budget
        {
            bail!("Goal token budget is exhausted");
        }
        if let Some(max_rounds) = self.max_rounds
            && self.rounds_started >= max_rounds
        {
            bail!("Goal round limit is exhausted");
        }
        if self.status == GoalStatus::Complete {
            bail!("a completed Goal cannot be resumed");
        }
        Ok(())
    }
}

/// Fold one goal event onto the current state. Every rule is a hard invariant
/// of the durable stream; a violation is an error, never a silent fallback.
pub fn apply_goal_event(state: &GoalFoldState, event: &GoalEvent) -> Result<GoalFoldState> {
    if event.version != GOAL_EVENT_VERSION {
        bail!("unsupported goal event version {}", event.version);
    }
    match &event.kind {
        GoalEventKind::Created {
            goal, created_at, ..
        } => {
            if goal.revision != 1 {
                bail!("goal_created revision must be 1, got {}", goal.revision);
            }
            if goal.rounds_started != 0 {
                bail!("goal_created rounds_started must be 0");
            }
            if goal.status != GoalStatus::Active {
                bail!(
                    "goal_created status must be active, got {}",
                    goal.status.as_str()
                );
            }
            if goal.tokens_used != 0 {
                bail!("goal_created tokens_used must be 0");
            }
            if let Some(current) = &state.current
                && current.status != GoalStatus::Complete
            {
                bail!("cannot create a Goal while an unfinished Goal exists");
            }
            if goal.created_at != *created_at {
                bail!("goal_created created_at mismatch");
            }
            Ok(GoalFoldState {
                current: Some(goal.clone()),
            })
        }
        GoalEventKind::Updated {
            operation, goal, ..
        } => {
            let Some(current) = &state.current else {
                bail!("goal_updated without a current Goal");
            };
            if current.goal_id != goal.goal_id {
                bail!("goal_updated changed the goal id");
            }
            if goal.revision != current.revision + 1 {
                bail!(
                    "goal_updated revision must be {}, got {}",
                    current.revision + 1,
                    goal.revision
                );
            }
            if goal.created_at != current.created_at {
                bail!("goal_updated changed created_at");
            }
            if goal.rounds_started != current.rounds_started {
                bail!("goal_updated changed rounds_started");
            }
            if goal.updated_at < current.updated_at {
                bail!("goal_updated updated_at regressed");
            }
            match operation {
                GoalOperation::Edit => {
                    if goal.status != current.status {
                        bail!("edit cannot change the status");
                    }
                    if goal.blocked_reason != current.blocked_reason {
                        bail!("edit cannot change the blocked reason");
                    }
                }
                GoalOperation::Pause => {
                    if current.status != GoalStatus::Active || goal.status != GoalStatus::Paused {
                        bail!("pause must move active -> paused");
                    }
                    require_same_definition(current, goal)?;
                }
                GoalOperation::Resume => {
                    if !matches!(
                        current.status,
                        GoalStatus::Active
                            | GoalStatus::Paused
                            | GoalStatus::Blocked
                            | GoalStatus::BudgetLimited
                    ) || goal.status != GoalStatus::Active
                    {
                        bail!("resume must move a stopped goal -> active");
                    }
                    require_same_definition(current, goal)?;
                    if let Some(max_rounds) = goal.max_rounds
                        && goal.rounds_started >= max_rounds
                    {
                        bail!("resume beyond the round limit");
                    }
                    if goal.blocked_reason.is_some() {
                        bail!("resume must clear the blocked reason");
                    }
                }
                GoalOperation::Complete => {
                    if current.status == GoalStatus::Complete || goal.status != GoalStatus::Complete
                    {
                        bail!("complete must move a non-complete goal -> complete");
                    }
                    require_same_definition(current, goal)?;
                }
                GoalOperation::Block => {
                    if current.status != GoalStatus::Active || goal.status != GoalStatus::Blocked {
                        bail!("block must move active -> blocked");
                    }
                    require_same_definition(current, goal)?;
                    if goal.blocked_reason.is_none() {
                        bail!("block requires a blocked reason");
                    }
                }
            }
            Ok(GoalFoldState {
                current: Some(goal.clone()),
            })
        }
        GoalEventKind::Round {
            goal_id,
            revision,
            round,
            tokens_delta,
            admitted_at,
            ..
        } => {
            let Some(current) = &state.current else {
                bail!("goal_round without a current Goal");
            };
            if current.goal_id != *goal_id {
                bail!("goal_round for an unknown goal id");
            }
            if current.status != GoalStatus::Active {
                bail!("goal_round while the Goal is {}", current.status.as_str());
            }
            if current.revision != *revision {
                bail!("goal_round for a stale revision");
            }
            if *round != current.rounds_started + 1 {
                bail!(
                    "goal_round round must be {}, got {}",
                    current.rounds_started + 1,
                    round
                );
            }
            if let Some(max_rounds) = current.max_rounds
                && *round > max_rounds
            {
                bail!("goal_round exceeds the round limit of {max_rounds}");
            }
            let mut next = current.clone();
            next.rounds_started = *round;
            next.tokens_used = next.tokens_used.saturating_add(*tokens_delta);
            next.updated_at = *admitted_at;
            if next
                .token_budget
                .is_some_and(|budget| next.tokens_used >= budget)
            {
                next.status = GoalStatus::BudgetLimited;
                next.blocked_reason = Some(GoalBlockReason {
                    code: "budget-limited".into(),
                    message: "token budget exhausted".into(),
                });
            }
            Ok(GoalFoldState {
                current: Some(next),
            })
        }
        GoalEventKind::Cleared {
            goal_id, revision, ..
        } => {
            let Some(current) = &state.current else {
                bail!("goal_cleared without a current Goal");
            };
            if current.goal_id != *goal_id {
                bail!("goal_cleared for an unknown goal id");
            }
            if current.revision + 1 != *revision {
                bail!("goal_cleared must reference the current revision + 1");
            }
            Ok(GoalFoldState { current: None })
        }
    }
}

/// Pause/resume/complete/block must preserve the goal definition (objective,
/// budget, round cap) — the DSH `requireSameDefinition` rule.
fn require_same_definition(current: &ThreadGoal, next: &ThreadGoal) -> Result<()> {
    if current.objective != next.objective
        || current.token_budget != next.token_budget
        || current.max_rounds != next.max_rounds
    {
        bail!("status transitions cannot change the goal definition");
    }
    Ok(())
}

/// The fold of a thread's goal event stream. `current` is the last snapshot,
/// or `None` after a tombstone or on an empty stream.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GoalFoldState {
    pub current: Option<ThreadGoal>,
}

/// Strictly fold an ordered `(event_type, data_json)` stream. The column type
/// must match the payload's self-describing `event` tag, and every event must
/// satisfy `apply_goal_event`.
pub fn fold_goal_events(events: &[(String, String)]) -> Result<GoalFoldState> {
    let mut state = GoalFoldState::default();
    for (event_type, data) in events {
        let event: GoalEvent = serde_json::from_str(data)
            .map_err(|error| anyhow::anyhow!("corrupt {event_type} event: {error}"))?;
        if event.event_type() != event_type {
            bail!(
                "goal event type mismatch: column {event_type}, payload {}",
                event.event_type()
            );
        }
        state = apply_goal_event(&state, &event)?;
    }
    Ok(state)
}

pub fn validate_objective(objective: String) -> Result<String> {
    let objective = objective.trim().to_string();
    if objective.is_empty() {
        bail!("Goal objective must not be empty");
    }
    if objective.chars().count() > MAX_OBJECTIVE_CHARS {
        bail!("Goal objective must be at most {MAX_OBJECTIVE_CHARS} characters");
    }
    Ok(objective)
}

pub fn validate_budget(token_budget: Option<u64>) -> Result<()> {
    if token_budget == Some(0) {
        bail!("Goal token budget must be a positive integer");
    }
    Ok(())
}

pub fn validate_max_rounds(max_rounds: Option<u64>) -> Result<()> {
    if max_rounds == Some(0) {
        bail!("Goal round limit must be a positive integer");
    }
    Ok(())
}

/// A blocker code is lower-kebab-case: starts with a lowercase letter, then
/// lowercase alphanumerics separated by single hyphens, no trailing hyphen.
pub fn validate_block_reason(reason: &GoalBlockReason) -> Result<()> {
    if !is_kebab_code(&reason.code) {
        bail!(
            "blocked reason code must be lower-kebab-case, got {:?}",
            reason.code
        );
    }
    if reason.message.trim().is_empty() {
        bail!("blocked reason message must not be empty");
    }
    Ok(())
}

fn is_kebab_code(code: &str) -> bool {
    let mut chars = code.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() {
        return false;
    }
    let mut after_dash = false;
    for c in chars {
        if c == '-' {
            if after_dash {
                return false;
            }
            after_dash = true;
        } else if c.is_ascii_alphanumeric() {
            after_dash = false;
        } else {
            return false;
        }
    }
    !after_dash
}

/// Goal-budget tokens exclude provider cache reads by definition.
pub fn budget_tokens(usage: crate::language_model::TokenUsage) -> u64 {
    usage.input_tokens + usage.cache_creation_input_tokens + usage.output_tokens
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language_model::TokenUsage;

    fn goal() -> ThreadGoal {
        ThreadGoal::new("thread".into(), "finish".into(), None, None).unwrap()
    }

    fn created_event(goal: &ThreadGoal) -> GoalEvent {
        GoalEvent {
            version: GOAL_EVENT_VERSION,
            kind: GoalEventKind::Created {
                actor: GoalActor::User,
                goal: goal.clone(),
                created_at: goal.created_at,
            },
        }
    }

    #[test]
    fn validates_objective_budget_and_rounds() {
        assert!(validate_objective("   ".into()).is_err());
        assert!(validate_objective("x".repeat(MAX_OBJECTIVE_CHARS + 1)).is_err());
        assert_eq!(validate_objective("  ship it  ".into()).unwrap(), "ship it");
        assert!(validate_budget(Some(0)).is_err());
        assert!(validate_budget(Some(1)).is_ok());
        assert!(validate_max_rounds(Some(0)).is_err());
        assert!(validate_max_rounds(Some(3)).is_ok());
        assert!(ThreadGoal::new("t".into(), "x".into(), None, Some(0)).is_err());
    }

    #[test]
    fn budget_excludes_cache_reads() {
        assert_eq!(
            budget_tokens(TokenUsage {
                input_tokens: 10,
                output_tokens: 4,
                cache_creation_input_tokens: 3,
                cache_read_input_tokens: 100,
            }),
            17
        );
    }

    #[test]
    fn status_machine_rejects_terminal_rewrites() {
        assert!(GoalStatus::Active.can_transition_to(GoalStatus::Complete));
        assert!(GoalStatus::Paused.can_transition_to(GoalStatus::Active));
        assert!(!GoalStatus::Complete.can_transition_to(GoalStatus::Active));
        assert!(!GoalStatus::Blocked.can_transition_to(GoalStatus::Complete));
    }

    #[test]
    fn block_reason_validation() {
        let ok = |code: &str| {
            validate_block_reason(&GoalBlockReason {
                code: code.into(),
                message: "why".into(),
            })
            .is_ok()
        };
        assert!(ok("round-limit"));
        assert!(ok("model-reported"));
        assert!(ok("a1-b2"));
        assert!(!ok("RoundLimit"));
        assert!(!ok("round_limit"));
        assert!(!ok("round--limit"));
        assert!(!ok("-round"));
        assert!(!ok("round-"));
        assert!(!ok(""));
        assert!(
            validate_block_reason(&GoalBlockReason {
                code: "round-limit".into(),
                message: "  ".into(),
            })
            .is_err()
        );
    }

    #[test]
    fn fold_applies_created_then_cleared() {
        let g = goal();
        let events = vec![
            (
                "goal_created".to_string(),
                serde_json::to_string(&created_event(&g)).unwrap(),
            ),
            (
                "goal_cleared".to_string(),
                serde_json::to_string(&GoalEvent {
                    version: GOAL_EVENT_VERSION,
                    kind: GoalEventKind::Cleared {
                        actor: GoalActor::User,
                        goal_id: g.goal_id.clone(),
                        revision: 2,
                        cleared_at: g.created_at + 10,
                    },
                })
                .unwrap(),
            ),
        ];
        let state = fold_goal_events(&events).unwrap();
        assert!(state.current.is_none());
    }

    #[test]
    fn fold_rejects_round_out_of_order() {
        let g = goal();
        let events = vec![
            (
                "goal_created".to_string(),
                serde_json::to_string(&created_event(&g)).unwrap(),
            ),
            (
                "goal_round".to_string(),
                serde_json::to_string(&GoalEvent {
                    version: GOAL_EVENT_VERSION,
                    kind: GoalEventKind::Round {
                        goal_id: g.goal_id.clone(),
                        revision: 1,
                        round: 2,
                        turn_id: "t".into(),
                        tokens_delta: 0,
                        admitted_at: g.created_at + 1,
                    },
                })
                .unwrap(),
            ),
        ];
        assert!(fold_goal_events(&events).is_err());
    }

    #[test]
    fn fold_rejects_stale_round_revision() {
        let g = goal();
        let mut second = goal();
        second.goal_id = g.goal_id.clone();
        second.created_at = g.created_at;
        second.revision = 2;
        second.updated_at = g.created_at + 5;
        let events = vec![
            (
                "goal_created".to_string(),
                serde_json::to_string(&created_event(&g)).unwrap(),
            ),
            (
                "goal_updated".to_string(),
                serde_json::to_string(&GoalEvent {
                    version: GOAL_EVENT_VERSION,
                    kind: GoalEventKind::Updated {
                        actor: GoalActor::User,
                        operation: GoalOperation::Pause,
                        goal: second.clone(),
                        turn_id: None,
                        created_at: second.updated_at,
                    },
                })
                .unwrap(),
            ),
            (
                "goal_round".to_string(),
                serde_json::to_string(&GoalEvent {
                    version: GOAL_EVENT_VERSION,
                    kind: GoalEventKind::Round {
                        goal_id: g.goal_id.clone(),
                        revision: 1,
                        round: 1,
                        turn_id: "t".into(),
                        tokens_delta: 0,
                        admitted_at: second.updated_at + 1,
                    },
                })
                .unwrap(),
            ),
        ];
        assert!(fold_goal_events(&events).is_err());
    }

    #[test]
    fn fold_rejects_round_when_paused() {
        let g = goal();
        let mut second = goal();
        second.goal_id = g.goal_id.clone();
        second.created_at = g.created_at;
        second.status = GoalStatus::Paused;
        second.blocked_reason = Some(GoalBlockReason {
            code: "user-paused".into(),
            message: "paused by user".into(),
        });
        second.revision = 2;
        second.updated_at = g.created_at + 5;
        let events = vec![
            (
                "goal_created".to_string(),
                serde_json::to_string(&created_event(&g)).unwrap(),
            ),
            (
                "goal_updated".to_string(),
                serde_json::to_string(&GoalEvent {
                    version: GOAL_EVENT_VERSION,
                    kind: GoalEventKind::Updated {
                        actor: GoalActor::User,
                        operation: GoalOperation::Pause,
                        goal: second.clone(),
                        turn_id: None,
                        created_at: second.updated_at,
                    },
                })
                .unwrap(),
            ),
            (
                "goal_round".to_string(),
                serde_json::to_string(&GoalEvent {
                    version: GOAL_EVENT_VERSION,
                    kind: GoalEventKind::Round {
                        goal_id: g.goal_id.clone(),
                        revision: 2,
                        round: 1,
                        turn_id: "t".into(),
                        tokens_delta: 0,
                        admitted_at: second.updated_at + 1,
                    },
                })
                .unwrap(),
            ),
        ];
        assert!(fold_goal_events(&events).is_err());
    }

    #[test]
    fn fold_rejects_round_cap_overflow() {
        let mut g = goal();
        g.max_rounds = Some(2);
        let events = vec![
            (
                "goal_created".to_string(),
                serde_json::to_string(&created_event(&g)).unwrap(),
            ),
            (
                "goal_round".to_string(),
                serde_json::to_string(&GoalEvent {
                    version: GOAL_EVENT_VERSION,
                    kind: GoalEventKind::Round {
                        goal_id: g.goal_id.clone(),
                        revision: 1,
                        round: 3,
                        turn_id: "t".into(),
                        tokens_delta: 0,
                        admitted_at: g.created_at + 1,
                    },
                })
                .unwrap(),
            ),
        ];
        assert!(fold_goal_events(&events).is_err());
    }

    #[test]
    fn fold_rejects_created_over_unfinished_goal() {
        let g = goal();
        let mut second = goal();
        second.created_at = g.created_at + 1;
        let events = vec![
            (
                "goal_created".to_string(),
                serde_json::to_string(&created_event(&g)).unwrap(),
            ),
            (
                "goal_created".to_string(),
                serde_json::to_string(&created_event(&second)).unwrap(),
            ),
        ];
        assert!(fold_goal_events(&events).is_err());
    }

    #[test]
    fn fold_rejects_discontinuous_revision() {
        let g = goal();
        let mut second = goal();
        second.goal_id = g.goal_id.clone();
        second.created_at = g.created_at;
        second.revision = 3; // should be 2
        second.updated_at = g.created_at + 5;
        let events = vec![
            (
                "goal_created".to_string(),
                serde_json::to_string(&created_event(&g)).unwrap(),
            ),
            (
                "goal_updated".to_string(),
                serde_json::to_string(&GoalEvent {
                    version: GOAL_EVENT_VERSION,
                    kind: GoalEventKind::Updated {
                        actor: GoalActor::User,
                        operation: GoalOperation::Pause,
                        goal: second,
                        turn_id: None,
                        created_at: g.created_at + 5,
                    },
                })
                .unwrap(),
            ),
        ];
        assert!(fold_goal_events(&events).is_err());
    }

    #[test]
    fn fold_round_advances_and_flips_budget_limit() {
        let mut g = goal();
        g.token_budget = Some(10);
        let events = vec![
            (
                "goal_created".to_string(),
                serde_json::to_string(&created_event(&g)).unwrap(),
            ),
            (
                "goal_round".to_string(),
                serde_json::to_string(&GoalEvent {
                    version: GOAL_EVENT_VERSION,
                    kind: GoalEventKind::Round {
                        goal_id: g.goal_id.clone(),
                        revision: 1,
                        round: 1,
                        turn_id: "t".into(),
                        tokens_delta: 12,
                        admitted_at: g.created_at + 10,
                    },
                })
                .unwrap(),
            ),
        ];
        let state = fold_goal_events(&events).unwrap();
        let current = state.current.unwrap();
        assert_eq!(current.rounds_started, 1);
        assert_eq!(current.tokens_used, 12);
        assert_eq!(current.status, GoalStatus::BudgetLimited);
        assert_eq!(
            current.blocked_reason.as_ref().unwrap().code,
            "budget-limited"
        );
        assert_eq!(current.updated_at, g.created_at + 10);
        assert_eq!(current.revision, 1); // rounds do not bump the revision
    }

    #[test]
    fn fold_rejects_unknown_event_version() {
        let g = goal();
        let mut event = created_event(&g);
        event.version = 99;
        let events = vec![(
            "goal_created".to_string(),
            serde_json::to_string(&event).unwrap(),
        )];
        assert!(fold_goal_events(&events).is_err());
    }

    #[test]
    fn fold_rejects_type_mismatch() {
        let g = goal();
        // Column says goal_cleared, payload is goal_created.
        let events = vec![(
            "goal_cleared".to_string(),
            serde_json::to_string(&created_event(&g)).unwrap(),
        )];
        assert!(fold_goal_events(&events).is_err());
    }

    #[test]
    fn resume_clears_blocked_reason_and_rejects_when_capped() {
        let g = goal();
        let mut blocked = goal();
        blocked.goal_id = g.goal_id.clone();
        blocked.created_at = g.created_at;
        blocked.status = GoalStatus::Blocked;
        blocked.blocked_reason = Some(GoalBlockReason {
            code: "model-reported".into(),
            message: "stuck".into(),
        });
        blocked.revision = 2;
        blocked.updated_at = g.created_at + 5;
        let mut resumed = blocked.clone();
        resumed.status = GoalStatus::Active;
        resumed.blocked_reason = None;
        resumed.revision = 3;
        resumed.updated_at = blocked.updated_at + 5;

        let base = fold_goal_events(&[(
            "goal_created".into(),
            serde_json::to_string(&created_event(&g)).unwrap(),
        )])
        .unwrap();
        let paused = apply_goal_event(
            &base,
            &GoalEvent {
                version: GOAL_EVENT_VERSION,
                kind: GoalEventKind::Updated {
                    actor: GoalActor::User,
                    operation: GoalOperation::Block,
                    goal: blocked.clone(),
                    turn_id: None,
                    created_at: blocked.updated_at,
                },
            },
        )
        .unwrap();
        let state = apply_goal_event(
            &paused,
            &GoalEvent {
                version: GOAL_EVENT_VERSION,
                kind: GoalEventKind::Updated {
                    actor: GoalActor::User,
                    operation: GoalOperation::Resume,
                    goal: resumed.clone(),
                    turn_id: None,
                    created_at: resumed.updated_at,
                },
            },
        )
        .unwrap();
        assert_eq!(state.current.as_ref().unwrap().status, GoalStatus::Active);

        // Resume beyond the round cap is rejected by the fold.
        let mut capped = resumed.clone();
        capped.rounds_started = 5;
        capped.max_rounds = Some(5);
        capped.revision = 4;
        capped.updated_at = resumed.updated_at + 1;
        assert!(
            apply_goal_event(
                &state,
                &GoalEvent {
                    version: GOAL_EVENT_VERSION,
                    kind: GoalEventKind::Updated {
                        actor: GoalActor::User,
                        operation: GoalOperation::Resume,
                        goal: capped,
                        turn_id: None,
                        created_at: resumed.updated_at + 1,
                    },
                },
            )
            .is_err()
        );
    }

    #[test]
    fn can_resume_enforces_both_caps() {
        let mut g = goal();
        g.tokens_used = 10;
        g.token_budget = Some(10);
        assert!(g.can_resume().is_err());
        let mut g = goal();
        g.rounds_started = 2;
        g.max_rounds = Some(2);
        assert!(g.can_resume().is_err());
        let g = goal();
        assert!(g.can_resume().is_ok());
    }
}
