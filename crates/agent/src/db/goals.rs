//! Transactional persistence and audit events for the current thread Goal.

use anyhow::{Context as _, Result, bail};
use rusqlite::{Connection, OptionalExtension as _, Transaction, params};
use serde::{Deserialize, Serialize};

use super::ThreadsDatabase;
use crate::goal::{GoalStatus, ThreadGoal, validate_budget, validate_objective};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalActor {
    User,
    Model,
    System,
}

pub fn create_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS thread_goals (
            thread_id TEXT PRIMARY KEY REFERENCES threads(id) ON DELETE CASCADE,
            goal_id TEXT NOT NULL UNIQUE,
            objective TEXT NOT NULL CHECK(length(trim(objective)) > 0 AND length(objective) <= 4000),
            status TEXT NOT NULL CHECK(status IN ('active', 'paused', 'blocked', 'budget_limited', 'complete')),
            token_budget INTEGER CHECK(token_budget IS NULL OR token_budget > 0),
            tokens_used INTEGER NOT NULL DEFAULT 0 CHECK(tokens_used >= 0),
            time_used_seconds INTEGER NOT NULL DEFAULT 0 CHECK(time_used_seconds >= 0),
            status_reason TEXT,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_thread_goals_status ON thread_goals(status);",
    )
    .context("create thread_goals table")?;
    Ok(())
}

impl ThreadsDatabase {
    pub fn load_goal(&self, thread_id: &str) -> Result<Option<ThreadGoal>> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        load_goal_from(&conn, thread_id)
    }

    /// Insert a new current Goal and its creation event atomically.
    pub fn create_goal(&self, goal: &ThreadGoal, actor: GoalActor) -> Result<()> {
        validate_objective(goal.objective.clone())?;
        validate_budget(goal.token_budget)?;
        let mut conn = self.conn.lock().expect("db mutex poisoned");
        let tx = conn
            .transaction()
            .context("begin Goal creation transaction")?;
        tx.execute(
            "INSERT INTO thread_goals
             (thread_id, goal_id, objective, status, token_budget, tokens_used,
              time_used_seconds, status_reason, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                goal.thread_id,
                goal.goal_id,
                goal.objective,
                goal.status.as_str(),
                goal.token_budget,
                goal.tokens_used,
                goal.time_used_seconds,
                goal.status_reason,
                goal.created_at,
                goal.updated_at,
            ],
        )
        .context("insert thread Goal")?;
        append_event(
            &tx,
            &goal.thread_id,
            "goal_created",
            serde_json::json!({"actor": actor, "goal": goal}),
        )?;
        tx.commit().context("commit Goal creation transaction")
    }

    /// Replace an existing Goal with a fresh id and reset accounting in one
    /// transaction. The explicit expected id is the user's confirmation fence.
    pub fn replace_goal(
        &self,
        expected_goal_id: &str,
        replacement: &ThreadGoal,
        actor: GoalActor,
        token_delta: u64,
        time_delta_seconds: u64,
        turn_id: Option<&str>,
    ) -> Result<()> {
        validate_objective(replacement.objective.clone())?;
        validate_budget(replacement.token_budget)?;
        let mut conn = self.conn.lock().expect("db mutex poisoned");
        let tx = conn
            .transaction()
            .context("begin Goal replacement transaction")?;
        let before = load_goal_from(&tx, &replacement.thread_id)?
            .ok_or_else(|| anyhow::anyhow!("thread has no current Goal"))?;
        if before.goal_id != expected_goal_id {
            bail!("stale Goal replacement for {expected_goal_id}");
        }
        if token_delta > 0 || time_delta_seconds > 0 {
            append_event(
                &tx,
                &replacement.thread_id,
                "goal_accounted",
                serde_json::json!({
                    "actor": GoalActor::System,
                    "goal_id": expected_goal_id,
                    "turn_id": turn_id,
                    "token_delta": token_delta,
                    "time_delta_seconds": time_delta_seconds,
                }),
            )?;
        }
        tx.execute(
            "DELETE FROM thread_goals WHERE thread_id=?1 AND goal_id=?2",
            params![replacement.thread_id, expected_goal_id],
        )?;
        append_event(
            &tx,
            &replacement.thread_id,
            "goal_cleared",
            serde_json::json!({
                "actor": actor,
                "goal_id": expected_goal_id,
                "before_status": before.status,
                "accounting_delta": {
                    "tokens": token_delta,
                    "time_seconds": time_delta_seconds,
                },
            }),
        )?;
        tx.execute(
            "INSERT INTO thread_goals
             (thread_id, goal_id, objective, status, token_budget, tokens_used,
              time_used_seconds, status_reason, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                replacement.thread_id,
                replacement.goal_id,
                replacement.objective,
                replacement.status.as_str(),
                replacement.token_budget,
                replacement.tokens_used,
                replacement.time_used_seconds,
                replacement.status_reason,
                replacement.created_at,
                replacement.updated_at,
            ],
        )?;
        append_event(
            &tx,
            &replacement.thread_id,
            "goal_created",
            serde_json::json!({"actor": actor, "goal": replacement}),
        )?;
        tx.commit().context("commit Goal replacement transaction")
    }

    /// Replace the current snapshot using goal-id CAS and append an audit event.
    pub fn update_goal(
        &self,
        expected_goal_id: &str,
        goal: &ThreadGoal,
        actor: GoalActor,
        turn_id: Option<&str>,
    ) -> Result<()> {
        validate_objective(goal.objective.clone())?;
        validate_budget(goal.token_budget)?;
        let mut conn = self.conn.lock().expect("db mutex poisoned");
        let tx = conn
            .transaction()
            .context("begin Goal update transaction")?;
        let before = load_goal_from(&tx, &goal.thread_id)?
            .ok_or_else(|| anyhow::anyhow!("thread has no current Goal"))?;
        if before.goal_id != expected_goal_id || goal.goal_id != expected_goal_id {
            bail!("stale Goal update for {expected_goal_id}");
        }
        let changed = tx.execute(
            "UPDATE thread_goals SET objective=?1, status=?2, token_budget=?3,
             tokens_used=?4, time_used_seconds=?5, status_reason=?6, updated_at=?7
             WHERE thread_id=?8 AND goal_id=?9",
            params![
                goal.objective,
                goal.status.as_str(),
                goal.token_budget,
                goal.tokens_used,
                goal.time_used_seconds,
                goal.status_reason,
                goal.updated_at,
                goal.thread_id,
                expected_goal_id,
            ],
        )?;
        if changed != 1 {
            bail!("stale Goal update for {expected_goal_id}");
        }
        append_event(
            &tx,
            &goal.thread_id,
            "goal_updated",
            serde_json::json!({
                "actor": actor,
                "goal_id": goal.goal_id,
                "turn_id": turn_id,
                "before_status": before.status,
                "after_status": goal.status,
                "status_reason": goal.status_reason,
            }),
        )?;
        tx.commit().context("commit Goal update transaction")
    }

    /// Add token/time deltas with CAS and atomically enforce the budget state.
    pub fn account_goal(
        &self,
        thread_id: &str,
        expected_goal_id: &str,
        token_delta: u64,
        time_delta_seconds: u64,
        turn_id: Option<&str>,
    ) -> Result<ThreadGoal> {
        let mut conn = self.conn.lock().expect("db mutex poisoned");
        let tx = conn
            .transaction()
            .context("begin Goal accounting transaction")?;
        let mut goal = load_goal_from(&tx, thread_id)?
            .ok_or_else(|| anyhow::anyhow!("thread has no current Goal"))?;
        if goal.goal_id != expected_goal_id {
            bail!("stale Goal accounting for {expected_goal_id}");
        }
        goal.tokens_used = goal.tokens_used.saturating_add(token_delta);
        goal.time_used_seconds = goal.time_used_seconds.saturating_add(time_delta_seconds);
        if goal.status == GoalStatus::Active
            && goal
                .token_budget
                .is_some_and(|budget| goal.tokens_used >= budget)
        {
            goal.status = GoalStatus::BudgetLimited;
            goal.status_reason = Some("token budget exhausted".into());
        }
        goal.updated_at = chrono::Utc::now().timestamp();
        tx.execute(
            "UPDATE thread_goals SET status=?1, tokens_used=?2,
             time_used_seconds=?3, status_reason=?4, updated_at=?5
             WHERE thread_id=?6 AND goal_id=?7",
            params![
                goal.status.as_str(),
                goal.tokens_used,
                goal.time_used_seconds,
                goal.status_reason,
                goal.updated_at,
                thread_id,
                expected_goal_id,
            ],
        )?;
        append_event(
            &tx,
            thread_id,
            "goal_accounted",
            serde_json::json!({
                "actor": GoalActor::System,
                "goal_id": expected_goal_id,
                "turn_id": turn_id,
                "token_delta": token_delta,
                "time_delta_seconds": time_delta_seconds,
                "after_status": goal.status,
            }),
        )?;
        tx.commit().context("commit Goal accounting transaction")?;
        Ok(goal)
    }

    pub fn clear_goal(
        &self,
        thread_id: &str,
        expected_goal_id: &str,
        actor: GoalActor,
        token_delta: u64,
        time_delta_seconds: u64,
        turn_id: Option<&str>,
    ) -> Result<()> {
        let mut conn = self.conn.lock().expect("db mutex poisoned");
        let tx = conn.transaction().context("begin Goal clear transaction")?;
        let before = load_goal_from(&tx, thread_id)?
            .ok_or_else(|| anyhow::anyhow!("thread has no current Goal"))?;
        if before.goal_id != expected_goal_id {
            bail!("stale Goal clear for {expected_goal_id}");
        }
        if token_delta > 0 || time_delta_seconds > 0 {
            tx.execute(
                "UPDATE thread_goals SET tokens_used=tokens_used + ?1,
                 time_used_seconds=time_used_seconds + ?2, updated_at=unixepoch()
                 WHERE thread_id=?3 AND goal_id=?4",
                params![token_delta, time_delta_seconds, thread_id, expected_goal_id],
            )?;
            append_event(
                &tx,
                thread_id,
                "goal_accounted",
                serde_json::json!({
                    "actor": GoalActor::System,
                    "goal_id": expected_goal_id,
                    "turn_id": turn_id,
                    "token_delta": token_delta,
                    "time_delta_seconds": time_delta_seconds,
                }),
            )?;
        }
        tx.execute(
            "DELETE FROM thread_goals WHERE thread_id=?1 AND goal_id=?2",
            params![thread_id, expected_goal_id],
        )?;
        append_event(
            &tx,
            thread_id,
            "goal_cleared",
            serde_json::json!({
                "actor": actor,
                "goal_id": expected_goal_id,
                "before_status": before.status,
            }),
        )?;
        tx.commit().context("commit Goal clear transaction")
    }

    /// Restore is fail-safe: persisted Active state becomes Paused before the
    /// caller can construct a live entity, so startup never emits a BYOK call.
    pub fn restore_goal(&self, thread_id: &str) -> Result<Option<ThreadGoal>> {
        let Some(mut goal) = self.load_goal(thread_id)? else {
            return Ok(None);
        };
        if goal.status == GoalStatus::Active {
            goal.status = GoalStatus::Paused;
            goal.status_reason = Some("paused after application restart".into());
            goal.updated_at = chrono::Utc::now().timestamp();
            self.update_goal(&goal.goal_id.clone(), &goal, GoalActor::System, None)?;
        }
        Ok(Some(goal))
    }
}

fn load_goal_from(conn: &Connection, thread_id: &str) -> Result<Option<ThreadGoal>> {
    conn.query_row(
        "SELECT thread_id, goal_id, objective, status, token_budget, tokens_used,
         time_used_seconds, status_reason, created_at, updated_at
         FROM thread_goals WHERE thread_id=?1",
        params![thread_id],
        |row| {
            let status: String = row.get(3)?;
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                status,
                row.get::<_, Option<u64>>(4)?,
                row.get::<_, u64>(5)?,
                row.get::<_, u64>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, i64>(8)?,
                row.get::<_, i64>(9)?,
            ))
        },
    )
    .optional()?
    .map(|row| {
        Ok(ThreadGoal {
            thread_id: row.0,
            goal_id: row.1,
            objective: row.2,
            status: GoalStatus::parse(&row.3)?,
            token_budget: row.4,
            tokens_used: row.5,
            time_used_seconds: row.6,
            status_reason: row.7,
            created_at: row.8,
            updated_at: row.9,
        })
    })
    .transpose()
}

fn append_event(
    tx: &Transaction<'_>,
    thread_id: &str,
    event_type: &str,
    data: serde_json::Value,
) -> Result<()> {
    let next_seq: i64 = tx.query_row(
        "SELECT COALESCE(MAX(seq), 0) + 1 FROM thread_events WHERE thread_id=?1",
        params![thread_id],
        |row| row.get(0),
    )?;
    tx.execute(
        "INSERT INTO thread_events (thread_id, seq, event_type, data)
         VALUES (?1, ?2, ?3, ?4)",
        params![
            thread_id,
            next_seq,
            event_type,
            serde_json::to_string(&data)?
        ],
    )?;
    Ok(())
}
