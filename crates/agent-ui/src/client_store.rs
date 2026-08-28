//! Client-side mirror of a thread's state, fed by `ServerNote`s from the
//! AgentServer. γ-1's data foundation — a pure projection with no gpui
//! dependency, unit-testable headlessly. The gpui `Entity<ClientStore>`
//! wrapper + the ServerNote pump + the full read API land in γ-1b/γ-2.

use std::collections::HashMap;

use agent::{Message, ThreadId};
use manox_protocol::ServerNote;
use manox_protocol::server::{ThreadInfoPayload, TokenUsageSnapshot};
use serde_json::Value;

/// A client-side projection of one thread's state. Every field is set by a
/// `ServerNote` (no recomputation). `apply_server_note` is the sole mutator.
pub struct ClientStore {
    pub id: ThreadId,
    pub messages: Vec<Message>,
    pub display_history: Value,
    pub display_title: String,
    pub model_id: Option<String>,
    pub model_name: Option<String>,
    pub model: Option<serde_json::Value>,
    pub permission_mode: String,
    pub reasoning_effort: String,
    pub pinned: bool,
    pub archived: bool,
    pub depth: u32,
    pub agent_label: String,
    pub self_author: String,
    pub worktree_active: bool,
    pub worktree_path: Option<String>,
    pub branch: Option<String>,
    pub goal: Option<Value>,
    pub goal_elapsed_seconds: Option<u64>,
    pub plan_mode: bool,
    pub persisted_plan: Option<Value>,
    pub browser_suites: Vec<String>,
    pub history_phase: String,
    pub running: bool,
    pub has_interacted: bool,
    pub cwd: String,
    pub project: Option<String>,
    pub background_tasks: Vec<Value>,
    pub cumulative_usage: Option<TokenUsageSnapshot>,
    pub per_model_usage: HashMap<String, TokenUsageSnapshot>,
    pub last_token_usage: Option<TokenUsageSnapshot>,
    pub cumulative_cost: f64,
    pub per_model_cost: HashMap<String, f64>,
}

impl Default for ClientStore {
    fn default() -> Self {
        Self {
            id: ThreadId::default(),
            messages: Vec::new(),
            display_history: Value::Array(Vec::new()),
            display_title: String::new(),
            model_id: None,
            model_name: None,
            model: None,
            permission_mode: String::new(),
            reasoning_effort: String::new(),
            pinned: false,
            archived: false,
            depth: 0,
            agent_label: String::new(),
            self_author: String::new(),
            worktree_active: false,
            worktree_path: None,
            branch: None,
            goal: None,
            goal_elapsed_seconds: None,
            plan_mode: false,
            persisted_plan: None,
            browser_suites: Vec::new(),
            history_phase: String::new(),
            running: false,
            has_interacted: false,
            cwd: String::new(),
            project: None,
            background_tasks: Vec::new(),
            cumulative_usage: None,
            per_model_usage: HashMap::new(),
            last_token_usage: None,
            cumulative_cost: 0.0,
            per_model_cost: HashMap::new(),
        }
    }
}

impl ClientStore {
    /// Apply one `ServerNote`, updating the mirrored state.
    pub fn apply_server_note(&mut self, note: &ServerNote) {
        match note {
            ServerNote::ThreadInfo { info, .. } => self.apply_thread_info(info),
            ServerNote::ThreadHistory {
                messages,
                display_history,
                ..
            } => {
                if let Ok(msgs) = serde_json::from_value::<Vec<Message>>(messages.clone()) {
                    self.messages = msgs;
                }
                self.display_history = display_history.clone();
            }
            ServerNote::TurnStarted { .. } => self.running = true,
            ServerNote::TurnFinished { .. } => self.running = false,
            ServerNote::CurrentModel { id, name, .. } => {
                self.model_id = id.clone();
                self.model_name = name.clone();
            }
            ServerNote::PermissionModeChanged { mode, .. } => {
                self.permission_mode = mode.clone();
            }
            ServerNote::ReasoningEffortChanged { effort, .. } => {
                self.reasoning_effort = effort.clone();
            }
            ServerNote::PlanModeChanged { enabled, .. } => self.plan_mode = *enabled,
            ServerNote::PlanUpdated { snapshot, .. } => self.persisted_plan = snapshot.clone(),
            ServerNote::GoalChanged { snapshot, .. } => self.goal = snapshot.clone(),
            ServerNote::WorktreeChanged { active, path, .. } => {
                self.worktree_active = *active;
                self.worktree_path = path.clone();
            }
            ServerNote::Branch { branch, .. } => self.branch = Some(branch.clone()),
            ServerNote::BackgroundTaskUpdated { snapshot, .. } => {
                if let Some(obj) = snapshot.as_object()
                    && let Some(id) = obj.get("task_id").and_then(Value::as_str)
                {
                    if let Some(idx) = self
                        .background_tasks
                        .iter()
                        .position(|t| t.get("task_id").and_then(Value::as_str) == Some(id))
                    {
                        self.background_tasks[idx] = snapshot.clone();
                    } else {
                        self.background_tasks.push(snapshot.clone());
                    }
                }
            }
            ServerNote::UsageSnapshot {
                cumulative,
                per_model,
                cumulative_cost,
                per_model_cost,
                ..
            } => {
                self.cumulative_usage = Some(cumulative.clone());
                self.per_model_usage = per_model.clone();
                self.cumulative_cost = *cumulative_cost;
                self.per_model_cost = per_model_cost.clone();
            }
            ServerNote::TokenUsage {
                input,
                output,
                cache_creation,
                cache_read,
                ..
            } => {
                self.last_token_usage = Some(TokenUsageSnapshot {
                    input: *input,
                    output: *output,
                    cache_creation: *cache_creation,
                    cache_read: *cache_read,
                });
            }
            _ => {}
        }
    }

    fn apply_thread_info(&mut self, info: &ThreadInfoPayload) {
        self.cwd = info.cwd.clone();
        self.project = info.project.clone();
        self.display_title = info.display_title.clone();
        self.model_id = info.model_id.clone();
        self.model_name = info.model_name.clone();
        self.model = info.model.clone();
        self.permission_mode = info.permission_mode.clone();
        self.reasoning_effort = info.reasoning_effort.clone();
        self.pinned = info.pinned;
        self.archived = info.archived;
        self.depth = info.depth;
        self.agent_label = info.agent_label.clone();
        self.self_author = info.self_author.clone();
        self.worktree_active = info.worktree_active;
        self.worktree_path = info.worktree_path.clone();
        self.branch = info.branch.clone();
        self.goal = info.goal.clone();
        self.goal_elapsed_seconds = info.goal_elapsed_seconds;
        self.plan_mode = info.plan_mode;
        self.browser_suites = info.browser_suites.clone();
        self.history_phase = info.history_phase.clone();
        self.running = info.running;
        self.has_interacted = info.has_interacted;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_payload() -> ThreadInfoPayload {
        ThreadInfoPayload {
            cwd: "/proj".into(),
            project: None,
            display_title: "Test".into(),
            model_id: Some("claude-sonnet".into()),
            model_name: Some("Sonnet".into()),
            model: None,
            permission_mode: "workspace-write".into(),
            reasoning_effort: "high".into(),
            pinned: false,
            archived: false,
            depth: 0,
            agent_label: "lead".into(),
            self_author: "lead".into(),
            worktree_active: false,
            worktree_path: None,
            branch: None,
            goal: None,
            goal_elapsed_seconds: None,
            plan_mode: false,
            browser_suites: vec![],
            history_phase: "ready".into(),
            running: false,
            has_interacted: false,
        }
    }

    #[test]
    fn thread_info_updates_all_fields() {
        let mut store = ClientStore::default();
        store.apply_server_note(&ServerNote::ThreadInfo {
            session_id: "s1".into(),
            info: Box::new(sample_payload()),
        });
        assert_eq!(store.cwd, "/proj");
        assert_eq!(store.display_title, "Test");
        assert_eq!(store.model_id.as_deref(), Some("claude-sonnet"));
        assert_eq!(store.permission_mode, "workspace-write");
        assert_eq!(store.reasoning_effort, "high");
        assert!(!store.running);
        assert!(!store.plan_mode);
        assert_eq!(store.self_author, "lead");
    }

    #[test]
    fn turn_started_finished_flip_running() {
        let mut store = ClientStore::default();
        store.apply_server_note(&ServerNote::TurnStarted {
            session_id: "s1".into(),
        });
        assert!(store.running);
        store.apply_server_note(&ServerNote::TurnFinished {
            session_id: "s1".into(),
            cancelled: false,
            failed: false,
            stranded_steer_ids: vec![],
        });
        assert!(!store.running);
    }

    #[test]
    fn plan_mode_changed_updates() {
        let mut store = ClientStore::default();
        store.apply_server_note(&ServerNote::PlanModeChanged {
            session_id: "s1".into(),
            enabled: true,
        });
        assert!(store.plan_mode);
    }

    #[test]
    fn usage_snapshot_sets_cumulative() {
        let mut store = ClientStore::default();
        store.apply_server_note(&ServerNote::UsageSnapshot {
            session_id: "s1".into(),
            cumulative: TokenUsageSnapshot {
                input: 100,
                output: 50,
                cache_creation: 0,
                cache_read: 0,
            },
            per_model: HashMap::new(),
            cumulative_cost: 0.01,
            per_model_cost: HashMap::new(),
        });
        assert_eq!(store.cumulative_usage.as_ref().unwrap().input, 100);
        assert!((store.cumulative_cost - 0.01).abs() < f64::EPSILON);
    }
}
