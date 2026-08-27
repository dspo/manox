//! Transitional gpui adapter around the gpui-free `agent::ThreadHandle`.
//!
//! The kernel `Thread` no longer lives in a gpui `Entity`: construction and
//! every method are cx-free, and state/events flow through a `ThreadHandle`
//! (`Arc` + lock + event channel, see `crates/agent/src/thread_pi.rs`). The
//! workspace still compiles against the old shape — `self.thread.read(cx).X()`,
//! `self.thread.update(cx, |t, _| t.m(a))`, `cx.subscribe(&self.thread, …)` —
//! so this adapter re-wraps the handle in a gpui `Entity`, forwards reads
//! (each getter returns an owned clone of the locked state, since the lock
//! cannot outlive the call) and mutations (through `with_mut`), and pumps the
//! handle's event channel into gpui `ThreadEvent`s so `cx.subscribe` keeps
//! working unchanged.
//!
//! Transitional adapter, removed in γ when agent-ui moves to a client store.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use agent::background_task::TaskSnapshot;
use agent::db::HistoryEntry;
use agent::goal::ThreadGoal;
use agent::language::Language;
use agent::language_model::{MessageContent, ReasoningEffort, TokenUsage};
use agent::message::MessageAuthor;
use agent::pi_engine::BrowserSuite;
use agent::plan::PlanSnapshot;
use agent::thread::{HistoryPhase, PermissionMode, ThreadHandle};
use agent::{Message, MessageUiMetadata, ThreadEvent, ThreadId};
use gpui::{Context, EventEmitter};
use pi::types::Model as PiModel;

/// Transitional adapter (removed in γ when agent-ui moves to a client store):
/// a gpui `Entity` that owns a gpui-free `ThreadHandle` and re-emits its
/// event channel as `ThreadEvent`s so the workspace's `cx.subscribe` still
/// works.
pub struct ThreadProxy {
    handle: ThreadHandle,
    /// Cached id — a `Thread`'s id is immutable for its lifetime, so mirroring
    /// `Thread::id` as a public field keeps `proxy.read(cx).id` call sites
    /// (field access) compiling unchanged against the handle.
    pub id: ThreadId,
    _pump: gpui::Task<()>,
}

impl EventEmitter<ThreadEvent> for ThreadProxy {}

impl ThreadProxy {
    /// Wrap a handle and start pumping its event channel into gpui events.
    pub fn new(handle: ThreadHandle, cx: &mut Context<Self>) -> Self {
        let rx = handle.subscribe();
        let _pump = cx.spawn(async move |this, cx: &mut gpui::AsyncApp| {
            while let Ok(ev) = rx.recv().await {
                // The broadcast clones one `Arc<ThreadEvent>` per event to
                // each subscriber. With a single subscriber the local `Arc`
                // is dropped at the end of the broadcast iteration, leaving
                // the channel's clone at refcount 1, so `try_unwrap` yields
                // the owned event for `cx.emit` (`ThreadEvent` is not
                // `Clone`). A second subscriber would leave refcount ≥ 2 and
                // the event would be dropped here — the proxy relies on
                // being the sole subscriber to its handle (true today: the
                // handle is never shared with session-core, which
                // materializes its own ThreadCore). Removed in γ when
                // agent-ui moves to a client store.
                if let Ok(ev) = Arc::try_unwrap(ev) {
                    let _ = this.update(cx, |_, cx| cx.emit(ev));
                } else {
                    tracing::warn!(
                        "ThreadProxy pump dropped event: multiple subscribers \
                         on the same ThreadHandle (assumed sole)"
                    );
                }
            }
        });
        let id = handle.read(|t| t.id.clone());
        Self { handle, id, _pump }
    }

    /// The wrapped gpui-free handle.
    pub fn handle(&self) -> &ThreadHandle {
        &self.handle
    }

    // ── Read forwarding ───────────────────────────────────────────────────
    // Each getter mirrors the `Thread` accessor of the same name and returns
    // an owned clone of the locked state (a reference cannot outlive the
    // `read` closure).

    pub fn id(&self) -> ThreadId {
        self.handle.read(|t| t.id.clone())
    }

    pub fn messages(&self) -> Vec<Message> {
        self.handle.read(|t| t.messages().to_vec())
    }

    pub fn display_history(&self) -> Vec<HistoryEntry> {
        self.handle.read(|t| t.display_history().to_vec())
    }

    pub fn request_token_usage(&self) -> HashMap<String, TokenUsage> {
        self.handle.read(|t| t.request_token_usage().clone())
    }

    pub fn last_request_token_usage(&self) -> Option<TokenUsage> {
        self.handle.read(|t| t.last_request_token_usage())
    }

    pub fn is_running(&self) -> bool {
        self.handle.read(|t| t.is_running())
    }

    pub fn history_phase(&self) -> HistoryPhase {
        self.handle.read(|t| t.history_phase())
    }

    pub fn cwd(&self) -> PathBuf {
        self.handle.read(|t| t.cwd().to_path_buf())
    }

    pub fn permission_mode(&self) -> PermissionMode {
        self.handle.read(|t| t.permission_mode())
    }

    pub fn reasoning_effort(&self) -> ReasoningEffort {
        self.handle.read(|t| t.reasoning_effort())
    }

    pub fn project(&self) -> Option<PathBuf> {
        self.handle.read(|t| t.project().cloned())
    }

    pub fn model(&self) -> Option<PiModel> {
        self.handle.read(|t| t.model().cloned())
    }

    pub fn plan_mode(&self) -> bool {
        self.handle.read(|t| t.plan_mode())
    }

    pub fn persisted_plan(&self) -> Option<PlanSnapshot> {
        self.handle.read(|t| t.persisted_plan().cloned())
    }

    pub fn goal(&self) -> Option<ThreadGoal> {
        self.handle.read(|t| t.goal())
    }

    pub fn goal_elapsed_seconds(&self) -> Option<u64> {
        self.handle.read(|t| t.goal_elapsed_seconds())
    }

    /// Path-shaped for the rail's `file_name()`/`display()` use; the kernel
    /// stores the worktree path as a string.
    pub fn worktree_path(&self) -> Option<PathBuf> {
        self.handle.read(|t| t.worktree_path().map(PathBuf::from))
    }

    pub fn pending_auth_entries(&self) -> Vec<(String, agent::permission::PendingAuthMeta)> {
        self.handle.read(|t| t.pending_auth_entries())
    }

    pub fn worktree_branch(&self) -> Option<String> {
        self.handle.read(|t| t.worktree().map(|w| w.branch.clone()))
    }

    pub fn browser_suites(&self) -> Vec<BrowserSuite> {
        self.handle.read(|t| t.browser_suites().to_vec())
    }

    pub fn background_task_snapshots(&self) -> Vec<TaskSnapshot> {
        self.handle.read(|t| t.background_task_snapshots())
    }

    pub fn has_interacted(&self) -> bool {
        self.handle.read(|t| t.has_interacted())
    }

    pub fn self_author(&self) -> MessageAuthor {
        self.handle.read(|t| t.self_author())
    }

    pub fn agent_language(&self) -> Language {
        self.handle.read(|t| t.agent_language())
    }

    pub fn display_title(&self) -> String {
        self.handle.read(|t| t.display_title())
    }

    pub fn cumulative_token_usage(&self) -> TokenUsage {
        self.handle.read(|t| t.cumulative_token_usage())
    }

    pub fn cumulative_cost(&self) -> f64 {
        self.handle.read(|t| t.cumulative_cost())
    }

    pub fn per_model_token_usage(&self) -> HashMap<String, TokenUsage> {
        self.handle.read(|t| t.per_model_token_usage())
    }

    pub fn per_model_cost(&self) -> HashMap<String, f64> {
        self.handle.read(|t| t.per_model_cost())
    }

    pub fn per_model_last_request_usage(&self) -> HashMap<String, TokenUsage> {
        self.handle.read(|t| t.per_model_last_request_usage())
    }

    // ── Mutation forwarding ───────────────────────────────────────────────
    // Every mutator takes `&self`: the underlying state lives behind the
    // handle's write lock, so the gpui entity itself never needs `&mut`.

    pub fn set_plan_review_pending(&self, pending: bool) {
        self.handle.with_mut(|t| t.set_plan_review_pending(pending));
    }

    pub fn run_turn(&self) {
        self.handle.with_mut(|t| t.run_turn());
    }

    pub fn cancel(&self) {
        self.handle.with_mut(|t| t.cancel());
    }

    pub fn set_archived(&self, archived: bool) {
        self.handle.with_mut(|t| t.set_archived(archived));
    }

    pub fn set_plan_mode(&self, enabled: bool) {
        self.handle.with_mut(|t| t.set_plan_mode(enabled));
    }

    pub fn set_model(&self, model: PiModel) {
        self.handle.with_mut(|t| t.set_model(model));
    }

    pub fn set_reasoning_effort(&self, effort: ReasoningEffort) {
        self.handle.with_mut(|t| t.set_reasoning_effort(effort));
    }

    pub fn set_permission_mode(&self, mode: PermissionMode) {
        self.handle.with_mut(|t| t.set_permission_mode(mode));
    }

    pub fn set_project(&self, dir: PathBuf) {
        self.handle.with_mut(|t| t.set_project(dir));
    }

    pub fn set_browser_suite(&self, suite: BrowserSuite, enable: bool) {
        self.handle.with_mut(|t| t.set_browser_suite(suite, enable));
    }

    pub fn submit_command(&self, name: &str, args: &str, ui: Option<MessageUiMetadata>) -> bool {
        self.handle.with_mut(|t| t.submit_command(name, args, ui))
    }

    pub fn submit_skill(&self, key: &str, args: &str, ui: Option<MessageUiMetadata>) -> bool {
        self.handle.with_mut(|t| t.submit_skill(key, args, ui))
    }

    pub fn insert_user_message_with_ui_metadata(
        &self,
        text: String,
        ui: Option<MessageUiMetadata>,
    ) {
        self.handle
            .with_mut(|t| t.insert_user_message_with_ui_metadata(text, ui));
    }

    pub fn insert_user_message_with_content_and_ui_metadata(
        &self,
        content: Vec<MessageContent>,
        ui: Option<MessageUiMetadata>,
    ) {
        self.handle
            .with_mut(|t| t.insert_user_message_with_content_and_ui_metadata(content, ui));
    }

    pub fn append_ui_note(&self, record: agent::db::UiNoteRecord) {
        self.handle.with_mut(|t| t.append_ui_note(record));
    }

    pub fn respond_authorization(&self, id: &str, response: agent::ToolAuthorizationResponse) {
        self.handle
            .with_mut(|t| t.respond_authorization(id, response));
    }

    pub fn enqueue_steer(
        &self,
        content: Vec<MessageContent>,
        ui: Option<MessageUiMetadata>,
    ) -> String {
        self.handle.with_mut(|t| t.enqueue_steer(content, ui))
    }

    pub fn cancel_pending_steer(&self, id: &str) -> bool {
        self.handle.with_mut(|t| t.cancel_pending_steer(id))
    }

    pub fn compact(&self, custom_instructions: Option<String>) {
        self.handle.with_mut(|t| t.compact(custom_instructions));
    }

    pub fn set_goal(&self, objective: String) -> anyhow::Result<()> {
        self.handle.with_mut(|t| t.set_goal(objective))
    }

    pub fn edit_goal(
        &self,
        objective: String,
        token_budget: Option<u64>,
        max_rounds: Option<u64>,
        actor: agent::goal::GoalActor,
    ) -> anyhow::Result<()> {
        self.handle
            .with_mut(|t| t.edit_goal(objective, token_budget, max_rounds, actor))
    }

    pub fn replace_goal(
        &self,
        objective: String,
        token_budget: Option<u64>,
        max_rounds: Option<u64>,
        actor: agent::goal::GoalActor,
    ) -> anyhow::Result<()> {
        self.handle
            .with_mut(|t| t.replace_goal(objective, token_budget, max_rounds, actor))
    }

    pub fn set_goal_status(
        &self,
        status: agent::goal::GoalStatus,
        reason: Option<agent::goal::GoalBlockReason>,
        actor: agent::goal::GoalActor,
    ) -> anyhow::Result<()> {
        self.handle
            .with_mut(|t| t.set_goal_status(status, reason, actor))
    }
    pub fn clear_goal(&self, actor: agent::goal::GoalActor) -> anyhow::Result<()> {
        self.handle.with_mut(|t| t.clear_goal(actor))
    }

    pub fn seed_plan_execution(
        &self,
        plan_file: String,
        seed_text: String,
        ui: Option<MessageUiMetadata>,
    ) {
        self.handle
            .with_mut(|t| t.seed_plan_execution(plan_file, seed_text, ui));
    }

    pub fn approve_plan(
        &self,
        compact: bool,
        compact_instructions: Option<String>,
        seed_text: String,
        ui: Option<MessageUiMetadata>,
    ) {
        self.handle
            .with_mut(|t| t.approve_plan(compact, compact_instructions, seed_text, ui));
    }

    /// Test-only: force the running flag so team-routing tests can simulate
    /// a busy thread without a live engine turn.
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_running_for_test(&self, running: bool) {
        self.handle.with_mut(|t| t.set_running_for_test(running));
    }
}
