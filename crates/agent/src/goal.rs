//! Persistent Goal domain model and lifecycle runtime.
//!
//! A Goal is the durable autonomy contract for one main thread. Completion is
//! reported explicitly through the Goal tools; there is deliberately no
//! per-turn evaluator or provider-specific cost accounting in this module.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

/// Maximum number of Unicode scalar values accepted for a Goal objective.
pub const MAX_OBJECTIVE_CHARS: usize = 4_000;

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

/// The one current Goal owned by a main thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadGoal {
    pub thread_id: String,
    pub goal_id: String,
    pub objective: String,
    pub status: GoalStatus,
    pub token_budget: Option<u64>,
    pub tokens_used: u64,
    pub time_used_seconds: u64,
    pub status_reason: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl ThreadGoal {
    pub fn new(thread_id: String, objective: String, token_budget: Option<u64>) -> Result<Self> {
        let objective = validate_objective(objective)?;
        validate_budget(token_budget)?;
        let now = chrono::Utc::now().timestamp();
        Ok(Self {
            thread_id,
            goal_id: uuid::Uuid::new_v4().to_string(),
            objective,
            status: GoalStatus::Active,
            token_budget,
            tokens_used: 0,
            time_used_seconds: 0,
            status_reason: None,
            created_at: now,
            updated_at: now,
        })
    }

    pub fn remaining_tokens(&self) -> Option<u64> {
        self.token_budget
            .map(|budget| budget.saturating_sub(self.tokens_used))
    }

    pub fn can_resume(&self) -> Result<()> {
        if let Some(budget) = self.token_budget
            && self.tokens_used >= budget
        {
            bail!("Goal token budget is exhausted");
        }
        if self.status == GoalStatus::Complete {
            bail!("a completed Goal cannot be resumed");
        }
        Ok(())
    }
}

/// In-memory coordinator for one persisted Goal.
///
/// `continuation_reserved` is the fencing bit for the idle gate: there can be
/// at most one automatic turn reservation for the current `goal_id`.
#[derive(Debug, Default)]
pub struct GoalRuntime {
    current: Option<ThreadGoal>,
    continuation_reserved: bool,
    continuation_failed: bool,
    blocker_reason: Option<String>,
    blocker_turns: u8,
    blocker_last_turn_id: Option<String>,
}

impl GoalRuntime {
    pub fn restore(goal: Option<ThreadGoal>) -> Self {
        Self {
            current: goal,
            continuation_reserved: false,
            continuation_failed: false,
            blocker_reason: None,
            blocker_turns: 0,
            blocker_last_turn_id: None,
        }
    }

    pub fn current(&self) -> Option<&ThreadGoal> {
        self.current.as_ref()
    }

    pub fn replace_snapshot(&mut self, goal: Option<ThreadGoal>) {
        let same_goal = self.current.as_ref().map(|goal| &goal.goal_id)
            == goal.as_ref().map(|goal| &goal.goal_id);
        self.current = goal;
        // A successfully persisted snapshot is the recovery fence after a
        // previous accounting failure. Explicit pause/edit/resume can thereby
        // re-enable automatic work without allowing an immediate blind retry.
        self.continuation_failed = false;
        if !same_goal {
            self.continuation_reserved = false;
            self.clear_blocker();
        }
    }

    pub fn expected_goal_id(&self) -> Option<&str> {
        self.current.as_ref().map(|goal| goal.goal_id.as_str())
    }

    pub fn reserve_continuation(&mut self, expected_goal_id: &str) -> bool {
        let active = self.current.as_ref().is_some_and(|goal| {
            goal.goal_id == expected_goal_id && goal.status == GoalStatus::Active
        });
        if !active || self.continuation_reserved || self.continuation_failed {
            return false;
        }
        self.continuation_reserved = true;
        true
    }

    pub fn release_continuation(&mut self, expected_goal_id: &str) {
        if self.expected_goal_id() == Some(expected_goal_id) {
            self.continuation_reserved = false;
        }
    }

    /// Fail closed after a persistence error while still releasing the active
    /// reservation. The next successfully persisted snapshot clears the latch.
    pub fn fail_continuation(&mut self, expected_goal_id: &str) {
        if self.expected_goal_id() == Some(expected_goal_id) {
            self.continuation_reserved = false;
            self.continuation_failed = true;
        }
    }

    /// Record at most one blocker report per Goal turn. A changed reason
    /// resets the consecutive-turn streak.
    pub fn record_blocker(&mut self, reason: &str, turn_id: &str) -> u8 {
        if self.blocker_last_turn_id.as_deref() == Some(turn_id) {
            return self.blocker_turns;
        }
        if self.blocker_reason.as_deref() == Some(reason) {
            self.blocker_turns = self.blocker_turns.saturating_add(1);
        } else {
            self.blocker_reason = Some(reason.to_string());
            self.blocker_turns = 1;
        }
        self.blocker_last_turn_id = Some(turn_id.to_string());
        self.blocker_turns
    }

    /// A Goal turn without a blocker report breaks the consecutive streak.
    pub fn finish_blocker_turn(&mut self, turn_id: &str) {
        if self.blocker_last_turn_id.as_deref() != Some(turn_id) {
            self.clear_blocker();
        }
    }

    pub fn clear_blocker(&mut self) {
        self.blocker_reason = None;
        self.blocker_turns = 0;
        self.blocker_last_turn_id = None;
    }
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

/// Goal-budget tokens exclude provider cache reads by definition.
pub fn budget_tokens(usage: crate::language_model::TokenUsage) -> u64 {
    usage.input_tokens + usage.cache_creation_input_tokens + usage.output_tokens
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language_model::TokenUsage;

    #[test]
    fn validates_objective_and_budget() {
        assert!(validate_objective("   ".into()).is_err());
        assert!(validate_objective("x".repeat(MAX_OBJECTIVE_CHARS + 1)).is_err());
        assert_eq!(validate_objective("  ship it  ".into()).unwrap(), "ship it");
        assert!(validate_budget(Some(0)).is_err());
        assert!(validate_budget(Some(1)).is_ok());
        assert!(validate_budget(None).is_ok());
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
    fn continuation_reservation_is_fenced_by_goal_id() {
        let goal = ThreadGoal::new("thread".into(), "finish".into(), None).unwrap();
        let id = goal.goal_id.clone();
        let mut runtime = GoalRuntime::restore(Some(goal));
        assert!(runtime.reserve_continuation(&id));
        assert!(!runtime.reserve_continuation(&id));
        runtime.release_continuation("stale");
        assert!(!runtime.reserve_continuation(&id));
        runtime.release_continuation(&id);
        assert!(runtime.reserve_continuation(&id));
    }

    #[test]
    fn persistence_failure_stops_until_a_successful_mutation() {
        let goal = ThreadGoal::new("thread".into(), "finish".into(), None).unwrap();
        let id = goal.goal_id.clone();
        let mut runtime = GoalRuntime::restore(Some(goal.clone()));
        assert!(runtime.reserve_continuation(&id));
        runtime.fail_continuation(&id);
        assert!(!runtime.reserve_continuation(&id));

        runtime.replace_snapshot(Some(goal));
        assert!(runtime.reserve_continuation(&id));
    }

    #[test]
    fn blocker_requires_three_distinct_consecutive_turns() {
        let goal = ThreadGoal::new("thread".into(), "finish".into(), None).unwrap();
        let mut runtime = GoalRuntime::restore(Some(goal));
        assert_eq!(runtime.record_blocker("waiting", "turn-1"), 1);
        assert_eq!(runtime.record_blocker("waiting", "turn-1"), 1);
        assert_eq!(runtime.record_blocker("waiting", "turn-2"), 2);
        assert_eq!(runtime.record_blocker("other", "turn-3"), 1);
        runtime.finish_blocker_turn("turn-4");
        assert_eq!(runtime.record_blocker("other", "turn-5"), 1);
        assert_eq!(runtime.record_blocker("other", "turn-6"), 2);
        assert_eq!(runtime.record_blocker("other", "turn-7"), 3);
    }

    #[test]
    fn status_machine_rejects_terminal_rewrites() {
        assert!(GoalStatus::Active.can_transition_to(GoalStatus::Complete));
        assert!(GoalStatus::Paused.can_transition_to(GoalStatus::Active));
        assert!(!GoalStatus::Complete.can_transition_to(GoalStatus::Active));
        assert!(!GoalStatus::Blocked.can_transition_to(GoalStatus::Complete));
    }
}
