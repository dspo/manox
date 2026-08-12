//! Goal `AgentTool` adapters for the pi harness (ported from the retired
//! manox harness). The engine actor owns no gpui entities, so the tools
//! operate through a [`GoalBridge`] shared with the thread facade: a Mutex
//! snapshot of the persisted [`ThreadGoal`] plus the db handle and the
//! notice channel for `GoalChanged` events.
//!
//! Model-side semantics mirror the retired tools: `CreateGoal` only when the
//! user explicitly requested autonomous goal work (fails while an unfinished
//! goal exists); `UpdateGoal` reports complete/blocked only — pause/resume/
//! replace/clear/budget stay user-side (`/goal`). Per-turn token accounting,
//! the autonomous continuation loop, and `BudgetLimited` enforcement are
//! follow-ups: the db accounting columns exist but stay untouched (deltas 0).

use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use pi::tool::{AgentTool, AgentToolResult, ToolContext, ToolError};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::db::{GoalActor, ThreadsDatabase};
use crate::goal::{GoalStatus, ThreadGoal};
use crate::thread::ThreadEvent;
use crate::thread_engine::BackendNotice;

/// Shared goal state between the thread facade (gpui thread, owns user-side
/// operations) and the goal tools (engine actor, tokio). All operations
/// persist to the db synchronously (local SQLite) and update the shared
/// snapshot; model-side operations additionally emit `GoalChanged` through
/// the engine notice channel, user-side operations emit on the facade.
pub struct GoalBridge {
    thread_id: String,
    goal: Mutex<Option<ThreadGoal>>,
    db: Arc<ThreadsDatabase>,
    notice_tx: Mutex<Option<mpsc::UnboundedSender<BackendNotice>>>,
}

impl GoalBridge {
    /// Open the shared db and build a bridge seeded with the thread's
    /// persisted goal (session-restore path). `None` when the db is
    /// unavailable — goal features degrade off rather than blocking launch.
    pub fn for_thread(thread_id: &str) -> Option<Arc<Self>> {
        let path = crate::db::default_db_path().ok()?;
        let db = match ThreadsDatabase::open(&path) {
            Ok(db) => Arc::new(db),
            Err(error) => {
                tracing::warn!("goal store unavailable ({error:#}); goal features disabled");
                return None;
            }
        };
        let initial = db.load_goal(thread_id).unwrap_or_else(|error| {
            tracing::warn!("goal load failed for {thread_id}: {error:#}");
            None
        });
        Some(Self::new(thread_id.to_string(), initial, db))
    }

    pub fn new(
        thread_id: String,
        initial: Option<ThreadGoal>,
        db: Arc<ThreadsDatabase>,
    ) -> Arc<Self> {
        Arc::new(Self {
            thread_id,
            goal: Mutex::new(initial),
            db,
            notice_tx: Mutex::new(None),
        })
    }

    /// The actor installs the notice sender once it starts.
    pub fn set_sender(&self, tx: mpsc::UnboundedSender<BackendNotice>) {
        *self.notice_tx.lock().unwrap() = Some(tx);
    }

    /// Current goal snapshot (tools + facade share one view).
    pub fn snapshot(&self) -> Option<ThreadGoal> {
        self.goal.lock().unwrap().clone()
    }

    /// Update the shared snapshot (facade mirrors its own writes here too,
    /// keeping the tool view honest).
    pub fn set(&self, goal: Option<ThreadGoal>) {
        *self.goal.lock().unwrap() = goal;
    }

    fn emit(&self, active: bool) {
        if let Some(tx) = self.notice_tx.lock().unwrap().as_ref() {
            let _ = tx.send(BackendNotice::Event(Box::new(ThreadEvent::GoalChanged {
                active,
            })));
        }
    }

    fn snapshot_json(&self) -> String {
        match self.snapshot() {
            Some(goal) => serde_json::to_string_pretty(&serde_json::json!({
                "goal": goal,
                "remaining_tokens": goal.remaining_tokens(),
            }))
            .expect("goal snapshot serializes"),
            None => "{\n  \"goal\": null\n}".to_string(),
        }
    }

    /// Create a fresh goal. Fails while an unfinished goal exists (terminal
    /// goals may be succeeded). Model tool passes `GoalActor::Model`; the
    /// facade passes the acting side.
    fn create(
        &self,
        objective: String,
        token_budget: Option<u64>,
        actor: GoalActor,
    ) -> Result<ThreadGoal> {
        if let Some(existing) = self.snapshot()
            && !existing.status.is_terminal()
        {
            bail!("an unfinished Goal already exists; finish or clear it first");
        }
        let goal = ThreadGoal::new(self.thread_id.clone(), objective, token_budget)?;
        self.db.create_goal(&goal, actor)?;
        self.set(Some(goal.clone()));
        Ok(goal)
    }

    /// Edit objective/budget in place (keeps goal id and status).
    fn edit(
        &self,
        objective: String,
        token_budget: Option<u64>,
        actor: GoalActor,
    ) -> Result<ThreadGoal> {
        let Some(mut goal) = self.snapshot() else {
            bail!("thread has no Goal");
        };
        let objective = crate::goal::validate_objective(objective)?;
        crate::goal::validate_budget(token_budget)?;
        goal.objective = objective;
        goal.token_budget = token_budget;
        goal.updated_at = chrono::Utc::now().timestamp();
        let goal_id = goal.goal_id.clone();
        self.db.update_goal(&goal_id, &goal, actor, None)?;
        self.set(Some(goal.clone()));
        Ok(goal)
    }

    /// Replace with a brand-new goal (new id, Active). Requires an existing
    /// goal to replace.
    fn replace(
        &self,
        objective: String,
        token_budget: Option<u64>,
        actor: GoalActor,
    ) -> Result<ThreadGoal> {
        let Some(current) = self.snapshot() else {
            bail!("thread has no Goal to replace");
        };
        let replacement = ThreadGoal::new(self.thread_id.clone(), objective, token_budget)?;
        self.db
            .replace_goal(&current.goal_id, &replacement, actor, 0, 0, None)?;
        self.set(Some(replacement.clone()));
        Ok(replacement)
    }

    /// Status transition with the domain guards (valid transitions; resume
    /// additionally requires budget headroom).
    fn set_status(
        &self,
        status: GoalStatus,
        reason: Option<String>,
        actor: GoalActor,
    ) -> Result<ThreadGoal> {
        let Some(mut goal) = self.snapshot() else {
            bail!("thread has no Goal");
        };
        if !goal.status.can_transition_to(status) {
            bail!("cannot move Goal from {:?} to {status:?}", goal.status);
        }
        if status == GoalStatus::Active {
            goal.can_resume()?;
        }
        goal.status = status;
        goal.status_reason = reason;
        goal.updated_at = chrono::Utc::now().timestamp();
        let goal_id = goal.goal_id.clone();
        self.db.update_goal(&goal_id, &goal, actor, None)?;
        self.set(Some(goal.clone()));
        Ok(goal)
    }

    /// Clear the goal (row deleted; history stays in the audit trail).
    fn clear(&self, actor: GoalActor) -> Result<()> {
        let Some(goal) = self.snapshot() else {
            return Ok(());
        };
        self.db
            .clear_goal(&self.thread_id, &goal.goal_id, actor, 0, 0, None)?;
        self.set(None);
        Ok(())
    }

    // Facade-side (user operation) entry points. The model tools use the
    // private `create` / `model_update` with `GoalActor::Model`; user
    // operations emit `GoalChanged` on the facade rather than the notice
    // channel (the bridge may predate the engine).

    pub fn create_goal(
        &self,
        objective: String,
        token_budget: Option<u64>,
        actor: GoalActor,
    ) -> Result<ThreadGoal> {
        self.create(objective, token_budget, actor)
    }

    pub fn edit_goal(
        &self,
        objective: String,
        token_budget: Option<u64>,
        actor: GoalActor,
    ) -> Result<ThreadGoal> {
        self.edit(objective, token_budget, actor)
    }

    pub fn replace_goal(
        &self,
        objective: String,
        token_budget: Option<u64>,
        actor: GoalActor,
    ) -> Result<ThreadGoal> {
        self.replace(objective, token_budget, actor)
    }

    pub fn set_goal_status(
        &self,
        status: GoalStatus,
        reason: Option<String>,
        actor: GoalActor,
    ) -> Result<ThreadGoal> {
        self.set_status(status, reason, actor)
    }

    pub fn clear_goal(&self, actor: GoalActor) -> Result<()> {
        self.clear(actor)
    }

    /// Model-side status report: Complete clears the goal; Blocked persists
    /// the blocked state for the user to resume or replace.
    fn model_update(&self, status: GoalStatus, reason: Option<String>) -> Result<String> {
        match status {
            GoalStatus::Complete => {
                self.clear(GoalActor::Model)?;
                self.emit(false);
            }
            GoalStatus::Blocked => {
                self.set_status(status, reason, GoalActor::Model)?;
                self.emit(true);
            }
            other => bail!("UpdateGoal cannot report {other:?}"),
        }
        Ok(self.snapshot_json())
    }
}

// ─── inputs ─────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct EmptyInput {}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct CreateGoalInput {
    /// Concrete objective explicitly requested by the user or system/developer instructions.
    objective: String,
    /// Positive token budget. Omit unless the user explicitly requested one.
    #[serde(default)]
    token_budget: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum ModelGoalStatus {
    Complete,
    Blocked,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct UpdateGoalInput {
    status: ModelGoalStatus,
    #[serde(default)]
    reason: Option<String>,
}

fn schema<T: JsonSchema>() -> serde_json::Value {
    let mut value = serde_json::to_value(schemars::schema_for!(T)).expect("schema serialization");
    if let Some(obj) = value.as_object_mut() {
        obj.remove("$schema");
        obj.remove("$defs");
    }
    value
}

// ─── tools ──────────────────────────────────────────────────────────────────

pub struct GetGoalTool {
    bridge: Arc<GoalBridge>,
}

pub struct CreateGoalTool {
    bridge: Arc<GoalBridge>,
}

pub struct UpdateGoalTool {
    bridge: Arc<GoalBridge>,
}

impl GetGoalTool {
    pub fn new(bridge: Arc<GoalBridge>) -> Self {
        Self { bridge }
    }
}
impl CreateGoalTool {
    pub fn new(bridge: Arc<GoalBridge>) -> Self {
        Self { bridge }
    }
}
impl UpdateGoalTool {
    pub fn new(bridge: Arc<GoalBridge>) -> Self {
        Self { bridge }
    }
}

#[async_trait::async_trait]
impl AgentTool for GetGoalTool {
    fn name(&self) -> &str {
        crate::tools::GET_GOAL
    }
    fn description(&self) -> &str {
        "Return the current main-thread Goal snapshot, including status, objective, accounting, budget, and remaining tokens."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        schema::<EmptyInput>()
    }
    // Kept out of Plan mode together with the mutating Goal tools; Default's
    // registered tool list remains stable across every Goal state.
    fn is_read_only(&self) -> bool {
        false
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        _params: serde_json::Value,
        _signal: CancellationToken,
        _ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        Ok(AgentToolResult::text(self.bridge.snapshot_json()))
    }
}

#[async_trait::async_trait]
impl AgentTool for CreateGoalTool {
    fn name(&self) -> &str {
        crate::tools::CREATE_GOAL
    }
    fn description(&self) -> &str {
        "Create the persistent Goal only when the user or system/developer instructions explicitly request autonomous Goal work. Do not infer a Goal from an ordinary task. Omit token_budget unless explicitly requested. Fails while an unfinished Goal exists."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        schema::<CreateGoalInput>()
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        _signal: CancellationToken,
        _ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        let input: CreateGoalInput = serde_json::from_value(params)
            .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
        self.bridge
            .create(input.objective, input.token_budget, GoalActor::Model)
            .map_err(|e| ToolError::ExecutionFailed(format!("{e:#}")))?;
        self.bridge.emit(true);
        Ok(AgentToolResult::text(self.bridge.snapshot_json()))
    }
}

#[async_trait::async_trait]
impl AgentTool for UpdateGoalTool {
    fn name(&self) -> &str {
        crate::tools::UPDATE_GOAL
    }
    fn description(&self) -> &str {
        "Report the current Goal as complete or genuinely blocked. Before complete, verify every part of the objective against current tool results and repository state. Use blocked only after the same blocking condition persists for at least three Goal turns and progress requires user input or external state. This tool cannot pause, resume, replace, clear, or budget-limit a Goal."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        schema::<UpdateGoalInput>()
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        _signal: CancellationToken,
        _ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        let input: UpdateGoalInput = serde_json::from_value(params)
            .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
        let status = match input.status {
            ModelGoalStatus::Complete => GoalStatus::Complete,
            ModelGoalStatus::Blocked => GoalStatus::Blocked,
        };
        self.bridge
            .model_update(status, input.reason)
            .map(AgentToolResult::text)
            .map_err(|e| ToolError::ExecutionFailed(format!("{e:#}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Goals FK onto threads: seed the owning row before any goal op.
    fn thread_record(id: &str) -> crate::db::ThreadRecord {
        crate::db::ThreadRecord {
            id: id.into(),
            summary: String::new(),
            title: None,
            title_override: None,
            model_id: String::new(),
            provider_id: None,
            cwd: "/tmp".into(),
            project: String::new(),
            agent_language: "en".into(),
            approval_mode: 0,
            reasoning_effort: 0,
            depth: 0,
            parent_id: None,
            archived: false,
            pinned: false,
            created_at: 0,
            interacted_at: 0,
            updated_at: 0,
            session_started_at: 0,
            revision: 0,
            cumulative_token_usage: crate::language_model::TokenUsage::default(),
            messages: Vec::new(),
            request_token_usage: std::collections::HashMap::new(),
            per_model_token_usage: std::collections::HashMap::new(),
            background_tasks: Vec::new(),
            always_allowed_tools: Vec::new(),
        }
    }

    fn bridge_in(dir: &tempfile::TempDir) -> Arc<GoalBridge> {
        let db = ThreadsDatabase::open(&dir.path().join("threads.db")).expect("open temp db");
        db.upsert(&thread_record("t1"), true)
            .expect("seed thread row");
        GoalBridge::new("t1".into(), None, Arc::new(db))
    }

    #[test]
    fn create_then_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let bridge = bridge_in(&dir);
        let goal = bridge
            .create("ship it".into(), None, GoalActor::Model)
            .unwrap();
        assert_eq!(bridge.snapshot().unwrap().goal_id, goal.goal_id);
        assert!(bridge.snapshot_json().contains("ship it"));
    }

    #[test]
    fn create_while_unfinished_fails() {
        let dir = tempfile::tempdir().unwrap();
        let bridge = bridge_in(&dir);
        bridge
            .create("first".into(), None, GoalActor::Model)
            .unwrap();
        let err = bridge
            .create("second".into(), None, GoalActor::Model)
            .unwrap_err();
        assert!(err.to_string().contains("unfinished Goal"));
    }

    #[test]
    fn model_update_complete_clears_goal() {
        let dir = tempfile::tempdir().unwrap();
        let bridge = bridge_in(&dir);
        let goal = bridge
            .create("objective".into(), None, GoalActor::Model)
            .unwrap();
        let db = ThreadsDatabase::open(&dir.path().join("threads.db")).unwrap();
        assert!(db.load_goal("t1").unwrap().is_some());

        bridge.model_update(GoalStatus::Complete, None).unwrap();
        assert!(bridge.snapshot().is_none());
        assert!(db.load_goal("t1").unwrap().is_none());
        let _ = goal;
    }

    #[test]
    fn model_update_blocked_persists_state() {
        let dir = tempfile::tempdir().unwrap();
        let bridge = bridge_in(&dir);
        bridge
            .create("objective".into(), None, GoalActor::Model)
            .unwrap();
        bridge
            .model_update(GoalStatus::Blocked, Some("needs user".into()))
            .unwrap();
        let goal = bridge.snapshot().unwrap();
        assert_eq!(goal.status, GoalStatus::Blocked);
        assert_eq!(goal.status_reason.as_deref(), Some("needs user"));

        let db = ThreadsDatabase::open(&dir.path().join("threads.db")).unwrap();
        let persisted = db.load_goal("t1").unwrap().unwrap();
        assert_eq!(persisted.status, GoalStatus::Blocked);
    }

    #[test]
    fn edit_keeps_id_replace_makes_new_one() {
        let dir = tempfile::tempdir().unwrap();
        let bridge = bridge_in(&dir);
        let original = bridge.create("v1".into(), None, GoalActor::User).unwrap();
        let edited = bridge
            .edit("v2".into(), Some(1_000), GoalActor::User)
            .unwrap();
        assert_eq!(edited.goal_id, original.goal_id);
        assert_eq!(edited.token_budget, Some(1_000));

        let replaced = bridge.replace("v3".into(), None, GoalActor::User).unwrap();
        assert_ne!(replaced.goal_id, original.goal_id);
        assert_eq!(replaced.status, GoalStatus::Active);
    }

    #[test]
    fn status_transition_guards() {
        let dir = tempfile::tempdir().unwrap();
        let bridge = bridge_in(&dir);
        bridge
            .create("objective".into(), None, GoalActor::User)
            .unwrap();
        // Active -> Paused -> Active round trip.
        bridge
            .set_status(GoalStatus::Paused, Some("user".into()), GoalActor::User)
            .unwrap();
        bridge
            .set_status(GoalStatus::Active, None, GoalActor::User)
            .unwrap();
        // Terminal states accept no further transitions.
        bridge
            .set_status(GoalStatus::Complete, None, GoalActor::User)
            .unwrap();
        assert!(
            bridge
                .set_status(GoalStatus::Active, None, GoalActor::User)
                .is_err()
        );
    }

    #[test]
    fn clear_removes_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let bridge = bridge_in(&dir);
        bridge
            .create("objective".into(), None, GoalActor::User)
            .unwrap();
        bridge.clear(GoalActor::User).unwrap();
        assert!(bridge.snapshot().is_none());
        // Clearing again is an idempotent no-op.
        bridge.clear(GoalActor::User).unwrap();
    }
}
