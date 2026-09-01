//! Goal service bridge and `AgentTool` adapters for the pi harness.
//!
//! [`GoalBridge`] is the single writer of one thread's goal event stream: it
//! folds the durable events (`crate::goal::fold_goal_events`) into the current
//! [`ThreadGoal`], validates every mutation against the fold before appending
//! (fail-loud, never a silent fallback), and holds the process-local
//! continuation authority (`armed`) that the round driver gates on — the DSH
//! phase/activation split. The engine actor owns no gpui entities, so the
//! facade and the tools share one bridge per thread.
//!
//! Model-side semantics: `CreateGoal` only when the user explicitly requested
//! autonomous goal work (fails while an unfinished goal exists); `UpdateGoal`
//! reports complete/blocked only — pause/resume/replace/clear/budget/rounds
//! stay user-side (`/goal`). Per-round token accounting and `BudgetLimited`
//! enforcement are applied by the round driver through `account_round`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use manox_harness::tool::{AgentTool, AgentToolResult, ToolContext, ToolError};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::db::ThreadsDatabase;
use crate::goal::{
    BLOCKED_MIN_GOAL_ROUNDS, GOAL_EVENT_VERSION, GoalActor, GoalBlockReason, GoalEvent,
    GoalEventKind, GoalFoldState, GoalOperation, GoalStatus, ThreadGoal, apply_goal_event,
    validate_block_reason,
};
use crate::goal_driver::{render_goal_blocked_wrapup, render_goal_complete_wrapup};
use crate::thread::ThreadEvent;
use crate::thread_engine::BackendNotice;

/// Incremental fold cache: `seq` is the highest thread-event seq folded.
#[derive(Debug, Clone, Default)]
struct GoalFoldCache {
    seq: i64,
    state: GoalFoldState,
}

/// Shared goal state between the thread facade (gpui thread, owns user-side
/// operations) and the goal tools (engine actor, tokio). All operations fold
/// the durable event stream synchronously (local SQLite) and update the
/// shared cache; model-side operations additionally emit `GoalChanged`
/// through the engine notice channel, user-side operations emit on the
/// facade.
pub struct GoalBridge {
    thread_id: String,
    db: Arc<ThreadsDatabase>,
    notice_tx: Mutex<Option<mpsc::UnboundedSender<BackendNotice>>>,
    fold: Mutex<GoalFoldCache>,
    /// Process-local continuation authority (DSH activation): whether the
    /// round driver may queue another round. Never inherited on restore.
    ///
    /// Relaxed ordering is sufficient because every reader and writer runs
    /// on the actor's single thread (facade writes go through the same
    /// bridge methods, serialized by the fold mutex) — the "arm then gate
    /// immediately" cross-callsite visibility depends on that single-threaded
    /// drive model. Do not upgrade to SeqCst: it buys nothing and hides the
    /// invariant.
    armed: AtomicBool,
    /// Set while a goal round run is in flight; tools read it to decide the
    /// closing wrapup for `complete`/`blocked` reports. Same Relaxed
    /// rationale as `armed`: the engine sets it and settles it on the actor
    /// thread, and tool reads happen inside a run that only the actor drives.
    goal_round_active: AtomicBool,
}

impl GoalBridge {
    /// Open the shared db and build a bridge seeded with the thread's goal
    /// event fold (session-restore path). `None` when the db is unavailable —
    /// goal features degrade off rather than blocking launch. A persisted
    /// Active goal is durably paused here: activation is never inherited, and
    /// startup never resumes autonomous work on its own.
    pub fn for_thread(thread_id: &str) -> Option<Arc<Self>> {
        let path = crate::db::default_db_path().ok()?;
        let db = match ThreadsDatabase::open(&path) {
            Ok(db) => Arc::new(db),
            Err(error) => {
                tracing::warn!("goal store unavailable ({error:#}); goal features disabled");
                return None;
            }
        };
        let bridge = Arc::new(Self::new(thread_id.to_string(), db));
        if let Err(error) = bridge.restore() {
            tracing::warn!("goal restore failed for {thread_id}: {error:#}");
            return None;
        }
        Some(bridge)
    }

    fn new(thread_id: String, db: Arc<ThreadsDatabase>) -> Self {
        Self {
            thread_id,
            db,
            notice_tx: Mutex::new(None),
            fold: Mutex::new(GoalFoldCache::default()),
            armed: AtomicBool::new(false),
            goal_round_active: AtomicBool::new(false),
        }
    }

    /// Restore path: fold the durable stream, then durably pause a persisted
    /// Active goal so a restart never auto-resumes autonomous work.
    fn restore(&self) -> Result<()> {
        self.sync()?;
        let active = self
            .fold
            .lock()
            .unwrap()
            .state
            .current
            .as_ref()
            .is_some_and(|goal| goal.status == GoalStatus::Active);
        if active {
            self.set_status(
                GoalStatus::Paused,
                Some(GoalBlockReason {
                    code: "restart-paused".into(),
                    message: "paused after application restart".into(),
                }),
                GoalActor::System,
            )?;
        }
        Ok(())
    }

    /// The actor installs the notice sender once it starts.
    pub fn set_sender(&self, tx: mpsc::UnboundedSender<BackendNotice>) {
        *self.notice_tx.lock().unwrap() = Some(tx);
    }

    /// Fold events appended since the cached seq. A corrupt stream fails
    /// loudly and poisons every subsequent goal read.
    fn sync(&self) -> Result<()> {
        let seq = self.fold.lock().unwrap().seq;
        let events = self.db.goal_events(&self.thread_id, seq)?;
        if events.is_empty() {
            return Ok(());
        }
        let mut cache = self.fold.lock().unwrap();
        let mut state = cache.state.clone();
        for (event_seq, event_type, data) in events {
            let event: GoalEvent = serde_json::from_str(&data)
                .map_err(|error| anyhow::anyhow!("corrupt {event_type} goal event: {error}"))?;
            state = apply_goal_event(&state, &event)?;
            cache.seq = event_seq;
        }
        cache.state = state;
        Ok(())
    }

    /// Current goal projection. A corrupt log degrades to `None` (goal
    /// features off) with a warning; mutations surface the error instead.
    pub fn snapshot(&self) -> Option<ThreadGoal> {
        if let Err(error) = self.sync() {
            tracing::warn!("goal fold failed: {error:#}");
            return None;
        }
        self.fold.lock().unwrap().state.current.clone()
    }

    /// Whether the round driver may queue another round.
    pub fn armed(&self) -> bool {
        self.armed.load(Ordering::Relaxed)
    }

    /// Remove process-local continuation authority without a durable change
    /// (DSH `disarm`). The goal keeps its durable phase.
    pub fn disarm(&self) {
        self.armed.store(false, Ordering::Relaxed);
    }

    /// Mark whether a goal round run is in flight (engine-driven; tools read
    /// it to pick the closing wrapup).
    pub fn mark_goal_round_active(&self, active: bool) {
        self.goal_round_active.store(active, Ordering::Relaxed);
    }

    fn emit(&self, goal: Option<ThreadGoal>) {
        if let Some(tx) = self.notice_tx.lock().unwrap().as_ref() {
            let _ = tx.send(BackendNotice::Event(Box::new(ThreadEvent::GoalChanged {
                goal,
            })));
        }
    }

    /// Validate an event against the cached fold, persist it, and re-fold.
    fn append_event(&self, event: &GoalEvent) -> Result<()> {
        self.sync()?;
        {
            let cache = self.fold.lock().unwrap();
            apply_goal_event(&cache.state, event)?;
        }
        let data = serde_json::to_string(event)?;
        self.db
            .append_goal_events(&self.thread_id, &[(event.event_type(), &data)])?;
        self.sync()?;
        Ok(())
    }

    /// Whether a goal round run is in flight (the engine's settle path uses
    /// this to decide whether `settle_goal_round` admission applies).
    pub fn goal_round_active(&self) -> bool {
        self.goal_round_active.load(Ordering::Relaxed)
    }

    /// Validate and persist a batch atomically (replace = tombstone + create).
    fn append_events(&self, events: &[GoalEvent]) -> Result<()> {
        self.sync()?;
        let mut batch: Vec<(String, String)> = Vec::with_capacity(events.len());
        {
            let cache = self.fold.lock().unwrap();
            let mut state = cache.state.clone();
            for event in events {
                state = apply_goal_event(&state, event)?;
                batch.push((
                    event.event_type().to_string(),
                    serde_json::to_string(event)?,
                ));
            }
        }
        let refs: Vec<(&str, &str)> = batch
            .iter()
            .map(|(event_type, data)| (event_type.as_str(), data.as_str()))
            .collect();
        self.db.append_goal_events(&self.thread_id, &refs)?;
        self.sync()?;
        Ok(())
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
        max_rounds: Option<u64>,
        actor: GoalActor,
    ) -> Result<ThreadGoal> {
        self.sync()?;
        if self
            .fold
            .lock()
            .unwrap()
            .state
            .current
            .as_ref()
            .is_some_and(|goal| !goal.status.is_terminal())
        {
            bail!("an unfinished Goal already exists; finish or clear it first");
        }
        let goal = ThreadGoal::new(self.thread_id.clone(), objective, token_budget, max_rounds)?;
        let event = GoalEvent {
            version: GOAL_EVENT_VERSION,
            kind: GoalEventKind::Created {
                actor,
                goal: goal.clone(),
                created_at: goal.created_at,
            },
        };
        self.append_event(&event)?;
        self.armed.store(true, Ordering::Relaxed);
        Ok(goal)
    }

    /// Edit objective/budget/rounds in place (keeps goal id, status, and
    /// revision continuity). Continuation authority is preserved (DSH: edit
    /// does not re-arm or disarm).
    fn edit(
        &self,
        objective: String,
        token_budget: Option<u64>,
        max_rounds: Option<u64>,
        actor: GoalActor,
    ) -> Result<ThreadGoal> {
        self.sync()?;
        let mut goal = self
            .fold
            .lock()
            .unwrap()
            .state
            .current
            .clone()
            .ok_or_else(|| anyhow::anyhow!("thread has no Goal"))?;
        let objective = crate::goal::validate_objective(objective)?;
        crate::goal::validate_budget(token_budget)?;
        crate::goal::validate_max_rounds(max_rounds)?;
        goal.objective = objective;
        goal.token_budget = token_budget;
        goal.max_rounds = max_rounds;
        goal.revision += 1;
        goal.updated_at = chrono::Utc::now().timestamp();
        let event = GoalEvent {
            version: GOAL_EVENT_VERSION,
            kind: GoalEventKind::Updated {
                actor,
                operation: GoalOperation::Edit,
                goal: goal.clone(),
                turn_id: None,
                created_at: goal.updated_at,
            },
        };
        self.append_event(&event)?;
        Ok(goal)
    }

    /// Replace with a brand-new goal (new id, Active, revision 1) in one
    /// atomic batch: tombstone the current goal, then create the replacement.
    fn replace(
        &self,
        objective: String,
        token_budget: Option<u64>,
        max_rounds: Option<u64>,
        actor: GoalActor,
    ) -> Result<ThreadGoal> {
        self.sync()?;
        let current = self
            .fold
            .lock()
            .unwrap()
            .state
            .current
            .clone()
            .ok_or_else(|| anyhow::anyhow!("thread has no Goal to replace"))?;
        let replacement =
            ThreadGoal::new(self.thread_id.clone(), objective, token_budget, max_rounds)?;
        let cleared = GoalEvent {
            version: GOAL_EVENT_VERSION,
            kind: GoalEventKind::Cleared {
                actor,
                goal_id: current.goal_id.clone(),
                revision: current.revision + 1,
                cleared_at: chrono::Utc::now().timestamp(),
            },
        };
        let created = GoalEvent {
            version: GOAL_EVENT_VERSION,
            kind: GoalEventKind::Created {
                actor,
                goal: replacement.clone(),
                created_at: replacement.created_at,
            },
        };
        self.append_events(&[cleared, created])?;
        self.armed.store(true, Ordering::Relaxed);
        Ok(replacement)
    }

    /// Status transition with the domain guards. `BudgetLimited` is applied
    /// by the fold on round accounting, never through this method.
    pub(crate) fn set_status(
        &self,
        status: GoalStatus,
        reason: Option<GoalBlockReason>,
        actor: GoalActor,
    ) -> Result<ThreadGoal> {
        self.sync()?;
        let mut goal = self
            .fold
            .lock()
            .unwrap()
            .state
            .current
            .clone()
            .ok_or_else(|| anyhow::anyhow!("thread has no Goal"))?;
        if !goal.status.can_transition_to(status) {
            bail!("cannot move Goal from {:?} to {status:?}", goal.status);
        }
        if status == GoalStatus::Active {
            goal.can_resume()?;
        }
        let operation = match status {
            GoalStatus::Paused => GoalOperation::Pause,
            GoalStatus::Active => GoalOperation::Resume,
            GoalStatus::Complete => GoalOperation::Complete,
            GoalStatus::Blocked => GoalOperation::Block,
            GoalStatus::BudgetLimited => {
                bail!("BudgetLimited is applied by the fold, not set_status")
            }
        };
        if let Some(reason) = &reason {
            validate_block_reason(reason)?;
        }
        goal.blocked_reason = match status {
            GoalStatus::Paused | GoalStatus::Blocked => reason,
            GoalStatus::Active | GoalStatus::Complete => None,
            GoalStatus::BudgetLimited => unreachable!(),
        };
        goal.status = status;
        goal.revision += 1;
        goal.updated_at = chrono::Utc::now().timestamp();
        let event = GoalEvent {
            version: GOAL_EVENT_VERSION,
            kind: GoalEventKind::Updated {
                actor,
                operation,
                goal: goal.clone(),
                turn_id: None,
                created_at: goal.updated_at,
            },
        };
        self.append_event(&event)?;
        self.armed
            .store(status == GoalStatus::Active, Ordering::Relaxed);
        Ok(goal)
    }

    /// Clear the current goal (tombstone; history stays in the event stream).
    fn clear(&self, actor: GoalActor) -> Result<()> {
        self.sync()?;
        let Some(current) = self.fold.lock().unwrap().state.current.clone() else {
            return Ok(());
        };
        let event = GoalEvent {
            version: GOAL_EVENT_VERSION,
            kind: GoalEventKind::Cleared {
                actor,
                goal_id: current.goal_id.clone(),
                revision: current.revision + 1,
                cleared_at: chrono::Utc::now().timestamp(),
            },
        };
        self.append_event(&event)?;
        self.armed.store(false, Ordering::Relaxed);
        self.emit(None);
        Ok(())
    }

    /// Admit one completed goal round: validate round/revision/cap/status in
    /// the fold and apply its token delta (which may flip the goal to
    /// BudgetLimited). The round driver calls this once per settled round.
    pub(crate) fn account_round(
        &self,
        round: u64,
        revision: u64,
        goal_id: String,
        tokens_delta: u64,
    ) -> Result<ThreadGoal> {
        self.sync()?;
        let event = GoalEvent {
            version: GOAL_EVENT_VERSION,
            kind: GoalEventKind::Round {
                goal_id,
                revision,
                round,
                turn_id: String::new(),
                tokens_delta,
                admitted_at: chrono::Utc::now().timestamp(),
            },
        };
        self.append_event(&event)?;
        let goal = self
            .snapshot()
            .ok_or_else(|| anyhow::anyhow!("goal lost after round accounting"))?;
        // A round that exhausts the budget stops continuation: the durable
        // phase is terminal, so the armed flag must not outlive it.
        if goal.status != GoalStatus::Active {
            self.armed.store(false, Ordering::Relaxed);
        }
        self.emit(Some(goal.clone()));
        Ok(goal)
    }

    // Facade-side (user operation) entry points. The model tools use the
    // private `create` / `set_status` with `GoalActor::Model`; user operations
    // emit `GoalChanged` on the facade rather than the notice channel (the
    // bridge may predate the engine).

    pub fn create_goal(
        &self,
        objective: String,
        token_budget: Option<u64>,
        max_rounds: Option<u64>,
        actor: GoalActor,
    ) -> Result<ThreadGoal> {
        let goal = self.create(objective, token_budget, max_rounds, actor)?;
        self.emit(Some(goal.clone()));
        Ok(goal)
    }

    pub fn edit_goal(
        &self,
        objective: String,
        token_budget: Option<u64>,
        max_rounds: Option<u64>,
        actor: GoalActor,
    ) -> Result<ThreadGoal> {
        let goal = self.edit(objective, token_budget, max_rounds, actor)?;
        self.emit(Some(goal.clone()));
        Ok(goal)
    }

    pub fn replace_goal(
        &self,
        objective: String,
        token_budget: Option<u64>,
        max_rounds: Option<u64>,
        actor: GoalActor,
    ) -> Result<ThreadGoal> {
        let goal = self.replace(objective, token_budget, max_rounds, actor)?;
        self.emit(Some(goal.clone()));
        Ok(goal)
    }

    pub fn set_goal_status(
        &self,
        status: GoalStatus,
        reason: Option<GoalBlockReason>,
        actor: GoalActor,
    ) -> Result<ThreadGoal> {
        let goal = self.set_status(status, reason, actor)?;
        self.emit(Some(goal.clone()));
        Ok(goal)
    }

    pub fn clear_goal(&self, actor: GoalActor) -> Result<()> {
        self.clear(actor)
    }

    /// Model-side status report: Complete keeps a durable complete snapshot
    /// (the user clears it); Blocked persists the blocked state for the user
    /// to resume or replace. The closing wrapup is injected under goal-round
    /// authority so the model addresses the user before the run ends.
    fn model_update(&self, status: GoalStatus, reason: Option<String>) -> Result<String> {
        match status {
            GoalStatus::Complete => {
                let goal = self.set_status(GoalStatus::Complete, None, GoalActor::Model)?;
                self.emit(Some(goal.clone()));
                let text = if self.goal_round_active.load(Ordering::Relaxed) {
                    render_goal_complete_wrapup(&goal.objective)
                } else {
                    self.snapshot_json()
                };
                Ok(text)
            }
            GoalStatus::Blocked => {
                self.sync()?;
                let current = self
                    .fold
                    .lock()
                    .unwrap()
                    .state
                    .current
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("thread has no Goal"))?;
                if current.rounds_started < BLOCKED_MIN_GOAL_ROUNDS {
                    bail!(
                        "blocked requires at least {BLOCKED_MIN_GOAL_ROUNDS} goal rounds; \
                         current round is {}",
                        current.rounds_started
                    );
                }
                let message = reason.unwrap_or_default();
                let goal = self.set_status(
                    GoalStatus::Blocked,
                    Some(GoalBlockReason {
                        code: "model-reported".into(),
                        message: message.clone(),
                    }),
                    GoalActor::Model,
                )?;
                self.emit(Some(goal.clone()));
                let text = if self.goal_round_active.load(Ordering::Relaxed) {
                    render_goal_blocked_wrapup(&goal.objective, &message)
                } else {
                    self.snapshot_json()
                };
                Ok(text)
            }
            other => bail!("UpdateGoal cannot report {other:?}"),
        }
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
    /// Positive round cap for automatic continuation. Omitted = unbounded.
    #[serde(default)]
    max_rounds: Option<u64>,
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
        "Return the current main-thread Goal snapshot, including status, objective, accounting, budget, rounds, and remaining tokens."
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
        "Create the persistent Goal only when the user or system/developer instructions explicitly request autonomous Goal work. Do not infer a Goal from an ordinary task. Omit token_budget unless explicitly requested. max_rounds caps the number of automatic continuation rounds; omit for unbounded. Fails while an unfinished Goal exists."
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
            .create(
                input.objective,
                input.token_budget,
                input.max_rounds,
                GoalActor::Model,
            )
            .map_err(|e| ToolError::ExecutionFailed(format!("{e:#}")))?;
        self.bridge.emit(self.bridge.snapshot());
        Ok(AgentToolResult::text(self.bridge.snapshot_json()))
    }
}

#[async_trait::async_trait]
impl AgentTool for UpdateGoalTool {
    fn name(&self) -> &str {
        crate::tools::UPDATE_GOAL
    }
    fn description(&self) -> &str {
        "Report the current Goal as complete or genuinely blocked. Before complete, verify every part of the objective against current tool results and repository state. Use blocked only after the same blocking condition persists for at least three Goal rounds and progress requires user input or external state. This tool cannot pause, resume, replace, clear, or budget-limit a Goal."
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
    use crate::goal::GoalEventKind;

    /// Seeds an in-memory db with the owning thread row and returns a bridge
    /// over it with a live notice channel.
    fn bridge_in(
        dir: &tempfile::TempDir,
    ) -> (Arc<GoalBridge>, mpsc::UnboundedReceiver<BackendNotice>) {
        let db = ThreadsDatabase::open(&dir.path().join("threads.db")).expect("open temp db");
        let (tx, rx) = mpsc::unbounded_channel();
        let bridge = GoalBridge::new("t1".into(), Arc::new(db));
        bridge.set_sender(tx);
        (Arc::new(bridge), rx)
    }

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
            tag: None,
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
        }
    }

    #[test]
    fn create_edit_snapshot_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let (bridge, _rx) = bridge_in(&dir);
        bridge.db.upsert(&thread_record("t1"), true).unwrap();
        let goal = bridge
            .create_goal("ship it".into(), Some(1_000), Some(5), GoalActor::Model)
            .unwrap();
        assert_eq!(goal.rounds_started, 0);
        assert_eq!(goal.revision, 1);
        assert!(bridge.armed());

        let edited = bridge
            .edit_goal("ship it better".into(), Some(2_000), None, GoalActor::User)
            .unwrap();
        assert_eq!(edited.revision, 2);
        assert_eq!(edited.max_rounds, None);
        assert!(bridge.snapshot_json().contains("rounds_started"));
    }

    #[test]
    fn create_while_unfinished_fails() {
        let dir = tempfile::tempdir().unwrap();
        let (bridge, _rx) = bridge_in(&dir);
        bridge.db.upsert(&thread_record("t1"), true).unwrap();
        bridge
            .create_goal("first".into(), None, None, GoalActor::Model)
            .unwrap();
        let err = bridge
            .create_goal("second".into(), None, None, GoalActor::Model)
            .unwrap_err();
        assert!(err.to_string().contains("unfinished Goal"));
    }

    #[test]
    fn account_round_advances_and_blocks_on_cap() {
        let dir = tempfile::tempdir().unwrap();
        let (bridge, _rx) = bridge_in(&dir);
        bridge.db.upsert(&thread_record("t1"), true).unwrap();
        let goal = bridge
            .create_goal("objective".into(), None, Some(2), GoalActor::User)
            .unwrap();
        let id = goal.goal_id.clone();
        let rev = goal.revision;
        let after = bridge.account_round(1, rev, id.clone(), 10).unwrap();
        assert_eq!(after.rounds_started, 1);
        assert_eq!(after.tokens_used, 10);
        assert_eq!(after.status, GoalStatus::Active);
        // Round 2 admitted; round 3 would overflow the cap and fail the fold.
        let after2 = bridge.account_round(2, rev, id.clone(), 5).unwrap();
        assert_eq!(after2.rounds_started, 2);
        assert!(bridge.account_round(3, rev, id.clone(), 0).is_err());
    }

    #[test]
    fn account_round_flips_budget_limited() {
        let dir = tempfile::tempdir().unwrap();
        let (bridge, _rx) = bridge_in(&dir);
        bridge.db.upsert(&thread_record("t1"), true).unwrap();
        let goal = bridge
            .create_goal("objective".into(), Some(10), None, GoalActor::User)
            .unwrap();
        let after = bridge
            .account_round(1, goal.revision, goal.goal_id.clone(), 12)
            .unwrap();
        assert_eq!(after.status, GoalStatus::BudgetLimited);
        assert_eq!(
            after.blocked_reason.as_ref().unwrap().code,
            "budget-limited"
        );
        assert!(!bridge.armed());
    }

    #[test]
    fn round_limit_auto_block_via_status() {
        let dir = tempfile::tempdir().unwrap();
        let (bridge, _rx) = bridge_in(&dir);
        bridge.db.upsert(&thread_record("t1"), true).unwrap();
        let goal = bridge
            .create_goal("objective".into(), None, Some(1), GoalActor::User)
            .unwrap();
        bridge
            .account_round(1, goal.revision, goal.goal_id.clone(), 0)
            .unwrap();
        // The driver blocks when rounds_started reaches the cap; this is the
        // bridge half of that decision.
        let blocked = bridge
            .set_goal_status(
                GoalStatus::Blocked,
                Some(GoalBlockReason {
                    code: "round-limit".into(),
                    message: "limit".into(),
                }),
                GoalActor::System,
            )
            .unwrap();
        assert_eq!(blocked.status, GoalStatus::Blocked);
        assert!(!bridge.armed());
    }

    #[test]
    fn model_blocked_requires_three_rounds() {
        let dir = tempfile::tempdir().unwrap();
        let (bridge, _rx) = bridge_in(&dir);
        bridge.db.upsert(&thread_record("t1"), true).unwrap();
        let goal = bridge
            .create_goal("objective".into(), None, None, GoalActor::User)
            .unwrap();
        // Fewer than 3 rounds: blocked is rejected.
        let err = bridge
            .model_update(GoalStatus::Blocked, Some("stuck".into()))
            .unwrap_err();
        assert!(err.to_string().contains("at least 3 goal rounds"));

        // Three admitted rounds: blocked succeeds with code model-reported.
        for round in 1..=3 {
            bridge
                .account_round(round, goal.revision, goal.goal_id.clone(), 0)
                .unwrap();
        }
        let text = bridge
            .model_update(GoalStatus::Blocked, Some("stuck".into()))
            .unwrap();
        assert!(text.contains("model_reported") || text.contains("goal"));
        let current = bridge.snapshot().unwrap();
        assert_eq!(current.status, GoalStatus::Blocked);
        assert_eq!(
            current.blocked_reason.as_ref().unwrap().code,
            "model-reported"
        );
    }

    #[test]
    fn model_complete_keeps_durable_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let (bridge, _rx) = bridge_in(&dir);
        bridge.db.upsert(&thread_record("t1"), true).unwrap();
        bridge
            .create_goal("objective".into(), None, None, GoalActor::User)
            .unwrap();
        let text = bridge.model_update(GoalStatus::Complete, None).unwrap();
        assert!(text.contains("goal"));
        let current = bridge.snapshot().unwrap();
        assert_eq!(current.status, GoalStatus::Complete);
        assert!(!bridge.armed());
    }

    #[test]
    fn replace_tombstones_then_creates() {
        let dir = tempfile::tempdir().unwrap();
        let (bridge, rx) = bridge_in(&dir);
        bridge.db.upsert(&thread_record("t1"), true).unwrap();
        let original = bridge
            .create_goal("v1".into(), None, None, GoalActor::User)
            .unwrap();
        let replacement = bridge
            .replace_goal("v2".into(), Some(1_000), Some(4), GoalActor::User)
            .unwrap();
        assert_ne!(replacement.goal_id, original.goal_id);
        assert_eq!(replacement.revision, 1);
        assert!(bridge.armed());
        // The replace batch emitted two GoalChanged notices (tombstone +
        // create are one transaction but the emit happens once on the facade
        // entry point; the internal append_events does not emit).
        let events = bridge.db.query_events("t1", None).unwrap();
        let types: Vec<String> = events.iter().map(|e| e.event_type.clone()).collect();
        assert_eq!(types, vec!["goal_created", "goal_cleared", "goal_created"]);
        drop(rx);
    }

    #[test]
    fn clear_removes_snapshot_and_disarms() {
        let dir = tempfile::tempdir().unwrap();
        let (bridge, _rx) = bridge_in(&dir);
        bridge.db.upsert(&thread_record("t1"), true).unwrap();
        bridge
            .create_goal("objective".into(), None, None, GoalActor::User)
            .unwrap();
        bridge.clear_goal(GoalActor::User).unwrap();
        assert!(bridge.snapshot().is_none());
        assert!(!bridge.armed());
        // Clearing again is an idempotent no-op.
        bridge.clear_goal(GoalActor::User).unwrap();
    }

    #[test]
    fn status_transition_guards() {
        let dir = tempfile::tempdir().unwrap();
        let (bridge, _rx) = bridge_in(&dir);
        bridge.db.upsert(&thread_record("t1"), true).unwrap();
        bridge
            .create_goal("objective".into(), None, None, GoalActor::User)
            .unwrap();
        bridge
            .set_goal_status(
                GoalStatus::Paused,
                Some(GoalBlockReason {
                    code: "user-paused".into(),
                    message: "paused by user".into(),
                }),
                GoalActor::User,
            )
            .unwrap();
        assert!(!bridge.armed());
        bridge
            .set_goal_status(GoalStatus::Active, None, GoalActor::User)
            .unwrap();
        assert!(bridge.armed());
        bridge
            .set_goal_status(GoalStatus::Complete, None, GoalActor::User)
            .unwrap();
        assert!(
            bridge
                .set_goal_status(GoalStatus::Active, None, GoalActor::User)
                .is_err()
        );
    }

    #[test]
    fn restore_pauses_active_goal_and_stays_disarmed() {
        let dir = tempfile::tempdir().unwrap();
        // Write an Active goal through a first bridge, then reopen it like a
        // session restore would.
        {
            let (bridge, _rx) = bridge_in(&dir);
            bridge.db.upsert(&thread_record("t1"), true).unwrap();
            bridge
                .create_goal("objective".into(), None, None, GoalActor::User)
                .unwrap();
            assert!(bridge.armed());
        }
        let db = ThreadsDatabase::open(&dir.path().join("threads.db")).unwrap();
        let restored = GoalBridge::new("t1".into(), Arc::new(db));
        restored.restore().unwrap();
        let goal = restored.snapshot().unwrap();
        assert_eq!(goal.status, GoalStatus::Paused);
        assert_eq!(goal.blocked_reason.as_ref().unwrap().code, "restart-paused");
        assert!(!restored.armed());
    }

    #[test]
    fn edit_preserves_armed_authority() {
        let dir = tempfile::tempdir().unwrap();
        let (bridge, _rx) = bridge_in(&dir);
        bridge.db.upsert(&thread_record("t1"), true).unwrap();
        bridge
            .create_goal("objective".into(), None, None, GoalActor::User)
            .unwrap();
        assert!(bridge.armed());
        bridge
            .edit_goal("new objective".into(), Some(50), Some(3), GoalActor::User)
            .unwrap();
        assert!(bridge.armed());
    }

    #[test]
    fn round_event_kind_serialization_is_stable() {
        let goal = ThreadGoal::new("t".into(), "x".into(), None, None).unwrap();
        let event = GoalEvent {
            version: GOAL_EVENT_VERSION,
            kind: GoalEventKind::Created {
                actor: GoalActor::User,
                goal: goal.clone(),
                created_at: goal.created_at,
            },
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"event\":\"created\""));
        let back: GoalEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back.event_type(), "goal_created");
    }
}
