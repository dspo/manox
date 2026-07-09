//! Team tools: shared coordination primitives available to the leader and
//! every worker member.
//!
//! All team tools are stateless — they read the owning thread's team (via
//! [`ToolContext::team`]) and label (via [`ToolContext::agent_label`]) at call
//! time, so the same tool instances serve the leader and all members. They run
//! synchronously (entity updates are in-memory) and return [`Task::ready`];
//! no background work, no `&dyn ToolContext` crossing an await.
//!
//! `requires_approval` is `false` throughout: creating a task or sending a
//! peer message is metadata, not a world-mutating action. Member tool calls
//! that DO mutate the world (write_file, bash, …) still bubble their own
//! authorizations up to the leader via the team's auth subscription.

use std::sync::Arc;

use gpui::{App, Task, WeakEntity};
use schemars::JsonSchema;
use serde::{Deserialize, de};
use tokio_util::sync::CancellationToken;

use crate::language_model::StopReason;
use crate::team::Team;
use crate::thread::{Thread, ThreadEvent};
use crate::tool::{AgentTool, AnyAgentTool, ToolContext};
use crate::tools::agent::{MemberSpec, spawn_team_member};

use super::task_list::TaskStatus;
use super::{LEADER_NAME, Member};

/// Tri-state owner deserializer. `Option<Option<String>>` as written collapses
/// JSON `null` to the outer `None` (serde's `Option` Visitor short-circuits
/// `null`), erasing the unassign intent — so the field is parsed by hand:
/// absent → `None` (leave unchanged), `null` → `Some(None)` (unassign back to
/// the pool), `"x"` → `Some(Some("x"))` (assign). `#[serde(default)]` covers
/// the absent case; this covers the present cases.
fn deserialize_owner<'de, D>(d: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = serde_json::Value::deserialize(d)?;
    match v {
        serde_json::Value::Null => Ok(Some(None)),
        serde_json::Value::String(s) => Ok(Some(Some(s))),
        other => Err(de::Error::custom(format!(
            "owner must be a string or null, got {other}"
        ))),
    }
}

// ─── helpers ──────────────────────────────────────────────────────────────

/// The owning thread's team, or an `Err` tool-result string naming the remedy.
/// Cloning the `Entity` is an `Arc` bump — cheap, and it lets the tool update
/// the team outside any borrow of `cx`.
fn team_or_err(ctx: &dyn ToolContext) -> Result<gpui::Entity<Team>, String> {
    ctx.team()
        .cloned()
        .ok_or_else(|| "no active team: create one with team_create first".to_string())
}

// ─── task_create ──────────────────────────────────────────────────────────

pub struct TaskCreateTool;

#[derive(Deserialize, JsonSchema)]
struct TaskCreateInput {
    /// One-line task title. The model uses this as the coordination handle.
    subject: String,
    /// Optional longer description / acceptance notes. Omit for a simple task.
    #[serde(default)]
    description: Option<String>,
}

impl AgentTool for TaskCreateTool {
    fn name(&self) -> &str {
        "task_create"
    }
    fn description(&self) -> &str {
        "Create a task on the shared team task list. Returns the new task id (e.g. T1). \
         Use for the team's coordination board — the leader breaks work into tasks, members \
         claim and update them. Requires an active team (team_create)."
    }
    fn input_schema(&self) -> serde_json::Value {
        crate::tools::schema::<TaskCreateInput>()
    }
    fn run(
        &self,
        input: serde_json::Value,
        _cancel: CancellationToken,
        ctx: &dyn ToolContext,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let Ok(parsed) = serde_json::from_value::<TaskCreateInput>(input) else {
            return Task::ready(Err("input parse failed".to_string()));
        };
        let team = match team_or_err(ctx) {
            Ok(t) => t,
            Err(e) => return Task::ready(Err(e)),
        };
        let tasks = team.read(cx).tasks().clone();
        let id = tasks.update(cx, |l, cx| l.create(parsed.subject, parsed.description, cx));
        Task::ready(Ok(format!("created task {id}")))
    }
}

// ─── task_list ────────────────────────────────────────────────────────────

pub struct TaskListTool;

#[derive(Deserialize, JsonSchema)]
struct TaskListInput {}

impl AgentTool for TaskListTool {
    fn name(&self) -> &str {
        "task_list"
    }
    fn description(&self) -> &str {
        "List all tasks on the shared team task list (id, status, owner, subject). \
         Read-only snapshot of the coordination board."
    }
    fn input_schema(&self) -> serde_json::Value {
        crate::tools::schema::<TaskListInput>()
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn run(
        &self,
        input: serde_json::Value,
        _cancel: CancellationToken,
        ctx: &dyn ToolContext,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let Ok(_) = serde_json::from_value::<TaskListInput>(input) else {
            return Task::ready(Err("input parse failed".to_string()));
        };
        let team = match team_or_err(ctx) {
            Ok(t) => t,
            Err(e) => return Task::ready(Err(e)),
        };
        let tasks = team.read(cx).tasks().clone();
        let snapshot: Vec<crate::team::Task> = tasks.update(cx, |l, _cx| l.tasks().to_vec());
        if snapshot.is_empty() {
            return Task::ready(Ok("(no tasks)".to_string()));
        }
        let mut lines = String::new();
        for t in &snapshot {
            let owner = t.owner.as_deref().unwrap_or("—");
            lines.push_str(&format!(
                "{} [{}] {} — {}\n",
                t.id, t.status, owner, t.subject
            ));
        }
        Task::ready(Ok(lines.trim_end().to_string()))
    }
}

// ─── task_update ──────────────────────────────────────────────────────────

pub struct TaskUpdateTool;

#[derive(Deserialize, JsonSchema)]
struct TaskUpdateInput {
    /// Id of the task to update (e.g. T1).
    id: String,
    /// New status. Omit to leave unchanged.
    #[serde(default)]
    status: Option<TaskStatus>,
    /// New owner (a member name). Tri-state: omit the field to leave unchanged;
    /// pass `null` to unassign back to the pool; pass a name to assign.
    #[serde(default, deserialize_with = "deserialize_owner")]
    owner: Option<Option<String>>,
    /// New subject. Omit to leave unchanged.
    #[serde(default)]
    subject: Option<String>,
}

impl AgentTool for TaskUpdateTool {
    fn name(&self) -> &str {
        "task_update"
    }
    fn description(&self) -> &str {
        "Update a task's status, owner, or subject. Each field is optional — omit a \
         field to leave it unchanged; for `owner`, pass null to unassign. Returns the \
         updated task or an error if the id is unknown."
    }
    fn input_schema(&self) -> serde_json::Value {
        crate::tools::schema::<TaskUpdateInput>()
    }
    fn run(
        &self,
        input: serde_json::Value,
        _cancel: CancellationToken,
        ctx: &dyn ToolContext,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let Ok(parsed) = serde_json::from_value::<TaskUpdateInput>(input) else {
            return Task::ready(Err("input parse failed".to_string()));
        };
        let team = match team_or_err(ctx) {
            Ok(t) => t,
            Err(e) => return Task::ready(Err(e)),
        };
        let tasks = team.read(cx).tasks().clone();
        let result = tasks.update(cx, |l, cx| {
            l.update(&parsed.id, parsed.status, parsed.owner, parsed.subject, cx)
        });
        match result {
            Ok(()) => {
                let task = tasks.read(cx).get(&parsed.id).cloned();
                match task {
                    Some(t) => {
                        let owner = t.owner.as_deref().unwrap_or("—");
                        Task::ready(Ok(format!(
                            "{} [{}] {} — {}",
                            t.id, t.status, owner, t.subject
                        )))
                    }
                    None => Task::ready(Ok(format!("updated task {}", parsed.id))),
                }
            }
            Err(e) => Task::ready(Err(e)),
        }
    }
}

// ─── task_get ─────────────────────────────────────────────────────────────

pub struct TaskGetTool;

#[derive(Deserialize, JsonSchema)]
struct TaskGetInput {
    /// Task id (e.g. T1).
    id: String,
}

impl AgentTool for TaskGetTool {
    fn name(&self) -> &str {
        "task_get"
    }
    fn description(&self) -> &str {
        "Read a single task by id (full subject, description, status, owner)."
    }
    fn input_schema(&self) -> serde_json::Value {
        crate::tools::schema::<TaskGetInput>()
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn run(
        &self,
        input: serde_json::Value,
        _cancel: CancellationToken,
        ctx: &dyn ToolContext,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let Ok(parsed) = serde_json::from_value::<TaskGetInput>(input) else {
            return Task::ready(Err("input parse failed".to_string()));
        };
        let team = match team_or_err(ctx) {
            Ok(t) => t,
            Err(e) => return Task::ready(Err(e)),
        };
        let tasks = team.read(cx).tasks().clone();
        match tasks.read(cx).get(&parsed.id) {
            Some(t) => {
                let owner = t.owner.as_deref().unwrap_or("(unassigned)");
                let desc = t.description.as_deref().unwrap_or("(no description)");
                Task::ready(Ok(format!(
                    "{} [{}] owner={}\nsubject: {}\ndescription: {}",
                    t.id, t.status, owner, t.subject, desc
                )))
            }
            None => Task::ready(Err(format!("task {} not found", parsed.id))),
        }
    }
}

// ─── send_message ─────────────────────────────────────────────────────────

pub struct SendMessageTool;

#[derive(Deserialize, JsonSchema)]
struct SendMessageInput {
    /// Recipient: a member name, "lead" (the leader / main thread), or "all"
    /// (broadcast to leader + every member except yourself).
    to: String,
    /// Message body. Delivered as a user-role message to the recipient's
    /// conversation, prefixed `[from {you}]`, so the recipient model sees it.
    message: String,
}

impl AgentTool for SendMessageTool {
    fn name(&self) -> &str {
        "send_message"
    }
    fn description(&self) -> &str {
        "Send a peer message to a teammate (\"lead\", a member name, or \"all\" for \
         broadcast). An idle recipient processes it immediately; a busy recipient's \
         message queues and flushes when its current turn ends. Use to hand off work, \
         report progress, or ask a clarifying question mid-flight."
    }
    fn input_schema(&self) -> serde_json::Value {
        crate::tools::schema::<SendMessageInput>()
    }
    fn run(
        &self,
        input: serde_json::Value,
        _cancel: CancellationToken,
        ctx: &dyn ToolContext,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let Ok(parsed) = serde_json::from_value::<SendMessageInput>(input) else {
            return Task::ready(Err("input parse failed".to_string()));
        };
        let team = match team_or_err(ctx) {
            Ok(t) => t,
            Err(e) => return Task::ready(Err(e)),
        };
        let from = ctx.agent_label().to_string();
        let result = team.update(cx, |t, cx| t.deliver(&from, &parsed.to, parsed.message, cx));
        match result {
            Ok(()) => Task::ready(Ok(format!("sent to {}", parsed.to))),
            Err(e) => Task::ready(Err(e)),
        }
    }
}

// ─── registry assembly ────────────────────────────────────────────────────

/// The shared team tools, registered for both the leader (in `main_registry`)
/// and every worker member (in `build_member_registry`). These are the
/// coordination primitives — `task_*` and `send_message`. Team-management
/// tools (`team_create` / `team_spawn` / `team_disband`) are leader-only and
/// appended separately.
pub fn shared_tools() -> Vec<AnyAgentTool> {
    vec![
        Arc::new(TaskCreateTool) as AnyAgentTool,
        Arc::new(TaskListTool) as AnyAgentTool,
        Arc::new(TaskUpdateTool) as AnyAgentTool,
        Arc::new(TaskGetTool) as AnyAgentTool,
        Arc::new(SendMessageTool) as AnyAgentTool,
    ]
}

// ─── leader-only team management ────────────────────────────────────────────

/// The leader's team-management tools (`team_create` / `team_spawn` /
/// `team_disband`), registered only in `main_registry`. Worker members never
/// form or disband teams — only the leader does. Each tool holds a weak handle
/// to the leader `Thread`; all spawning/teardown is synchronous entity work on
/// `&mut App`, so these return `Task::ready`.
pub fn leader_tools(leader: WeakEntity<Thread>) -> Vec<AnyAgentTool> {
    vec![
        Arc::new(TeamCreateTool {
            leader: leader.clone(),
        }) as AnyAgentTool,
        Arc::new(TeamSpawnTool {
            leader: leader.clone(),
        }) as AnyAgentTool,
        Arc::new(TeamDisbandTool { leader }) as AnyAgentTool,
    ]
}

/// Shared input shape for a member spec — used by both `team_create`'s roster
/// and `team_spawn`'s single-add path.
#[derive(Deserialize, JsonSchema)]
struct MemberSpecInput {
    /// Worker name (unique within the team; used as the routing handle for
    /// `send_message` and the auth-bubble composite id).
    name: String,
    /// Short role label shown in the roster UI (e.g. "explorer").
    role: String,
    /// Sub-agent definition to spawn (from `~/.config/cx/manox/agents/*.md`).
    subagent_type: String,
    /// The member's first task. Becomes its opening user message; the member
    /// has no access to the leader's conversation, so include any needed file
    /// paths, error text, or context here.
    prompt: String,
}

impl MemberSpecInput {
    fn into_spec(self) -> MemberSpec {
        MemberSpec {
            name: self.name,
            subagent_type: self.subagent_type,
            prompt: self.prompt,
        }
    }
}

pub struct TeamCreateTool {
    leader: WeakEntity<Thread>,
}

#[derive(Deserialize, JsonSchema)]
struct TeamCreateInput {
    /// Team name (display only).
    name: String,
    /// Initial roster of worker members to spawn alongside the team. Omit for
    /// an empty team you grow later with `team_spawn`.
    #[serde(default)]
    members: Vec<MemberSpecInput>,
}

impl AgentTool for TeamCreateTool {
    fn name(&self) -> &str {
        "team_create"
    }
    fn description(&self) -> &str {
        "Form a peer-agents team with you (the main agent) as leader and the \
         listed sub-agents as long-lived worker members. Members coordinate via \
         the shared task list and `send_message`; each member runs autonomously \
         to completion and reports back. Use for parallel sub-tasks that need to \
         coordinate or share progress — NOT for independent fire-and-forget work \
         (use the `agent` tool for that). Only one team may be active at a time; \
         disband with `team_disband` before forming another. Assign members \
         disjoint write ranges to avoid file-write lock contention."
    }
    fn input_schema(&self) -> serde_json::Value {
        crate::tools::schema::<TeamCreateInput>()
    }
    fn run(
        &self,
        input: serde_json::Value,
        _cancel: CancellationToken,
        _ctx: &dyn ToolContext,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let parsed = match serde_json::from_value::<TeamCreateInput>(input) {
            Ok(p) => p,
            Err(e) => return Task::ready(Err(format!("team_create input parse failed: {e}"))),
        };
        let Some(leader) = self.leader.upgrade() else {
            return Task::ready(Err("leader thread dropped".to_string()));
        };
        // Single-active-team guard: the team↔member ownership cycle relies on
        // at most one live team; a second would dangle the first's members.
        if leader.read(cx).team().is_some() {
            return Task::ready(Err(
                "a team is already active; disband it with team_disband first".to_string(),
            ));
        }
        let team = Team::new(parsed.name, leader.downgrade(), cx);
        leader.update(cx, |t, cx| t.set_team(team.clone(), cx));

        // Wire the leader's terminal-Stop → flush leader inbox. The leader is
        // mid-turn when team_create runs; this subscription fires on its
        // subsequent turn ends.
        let team_w = team.downgrade();
        let leader_sub =
            cx.subscribe(
                &leader,
                move |_leader, ev: &ThreadEvent, cx: &mut App| match ev {
                    ThreadEvent::Stop(StopReason::EndTurn)
                    | ThreadEvent::Stop(StopReason::MaxTokens)
                    | ThreadEvent::Stop(StopReason::Refusal) => {
                        let tw = team_w.clone();
                        cx.defer(move |cx| {
                            if let Some(t) = tw.upgrade() {
                                t.update(cx, |tm, cx| tm.flush_inbox(LEADER_NAME, cx));
                            }
                        });
                    }
                    ThreadEvent::Stop(StopReason::ToolUse) => {}
                    _ => {}
                },
            );
        team.update(cx, |t, _cx| t.set_leader_sub(leader_sub));

        // Spawn each member and fold it into the roster. A spawn failure aborts:
        // disband the partial team and clear the leader's team so the user can
        // retry cleanly rather than landing on a half-built team.
        for spec in parsed.members {
            let member_name = spec.name.clone();
            let role = spec.role.clone();
            match spawn_team_member(&self.leader, team.clone(), spec.into_spec(), cx) {
                Ok((thread, sub)) => {
                    let res = team.update(cx, |t, cx| {
                        t.insert_member(Member::new(member_name.clone(), role, thread), cx)
                    });
                    if let Err(e) = res {
                        team.update(cx, |t, cx| t.disband(cx));
                        leader.update(cx, |t, cx| t.clear_team(cx));
                        return Task::ready(Err(format!(
                            "team_create aborted: member '{member_name}' rejected: {e}"
                        )));
                    }
                    team.update(cx, |t, _cx| t.set_member_sub(member_name, sub));
                }
                Err(e) => {
                    team.update(cx, |t, cx| t.disband(cx));
                    leader.update(cx, |t, cx| t.clear_team(cx));
                    return Task::ready(Err(format!(
                        "team_create aborted: spawn of '{member_name}' failed: {e}"
                    )));
                }
            }
        }

        let count = team.read(cx).members().len();
        let name = team.read(cx).name().to_string();
        Task::ready(Ok(format!("team '{name}' created with {count} member(s)")))
    }
}

pub struct TeamSpawnTool {
    leader: WeakEntity<Thread>,
}

#[derive(Deserialize, JsonSchema)]
struct TeamSpawnInput {
    name: String,
    role: String,
    subagent_type: String,
    prompt: String,
}

impl TeamSpawnInput {
    fn into_spec(self) -> MemberSpec {
        MemberSpec {
            name: self.name,
            subagent_type: self.subagent_type,
            prompt: self.prompt,
        }
    }
}

impl AgentTool for TeamSpawnTool {
    fn name(&self) -> &str {
        "team_spawn"
    }
    fn description(&self) -> &str {
        "Add a worker member to the active team. The team must already exist \
         (team_create). The new member runs autonomously and reports back via \
         `send_message`. Refused if the roster is full (5 workers max)."
    }
    fn input_schema(&self) -> serde_json::Value {
        crate::tools::schema::<TeamSpawnInput>()
    }
    fn run(
        &self,
        input: serde_json::Value,
        _cancel: CancellationToken,
        _ctx: &dyn ToolContext,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let parsed = match serde_json::from_value::<TeamSpawnInput>(input) {
            Ok(p) => p,
            Err(e) => return Task::ready(Err(format!("team_spawn input parse failed: {e}"))),
        };
        let Some(leader) = self.leader.upgrade() else {
            return Task::ready(Err("leader thread dropped".to_string()));
        };
        let Some(team) = leader.read(cx).team().cloned() else {
            return Task::ready(Err(
                "no active team; create one with team_create".to_string()
            ));
        };
        if !team.read(cx).has_room() {
            return Task::ready(Err("team is full (5 workers max)".to_string()));
        }
        let member_name = parsed.name.clone();
        let role = parsed.role.clone();
        match spawn_team_member(&self.leader, team.clone(), parsed.into_spec(), cx) {
            Ok((thread, sub)) => {
                let res = team.update(cx, |t, cx| {
                    t.insert_member(Member::new(member_name.clone(), role, thread), cx)
                });
                if let Err(e) = res {
                    return Task::ready(Err(format!("spawn of '{member_name}' rejected: {e}")));
                }
                team.update(cx, |t, _cx| t.set_member_sub(member_name.clone(), sub));
                Task::ready(Ok(format!("spawned member '{member_name}'")))
            }
            Err(e) => Task::ready(Err(format!("spawn of '{member_name}' failed: {e}"))),
        }
    }
}

pub struct TeamDisbandTool {
    leader: WeakEntity<Thread>,
}

#[derive(Deserialize, JsonSchema)]
struct TeamDisbandInput {}

impl AgentTool for TeamDisbandTool {
    fn name(&self) -> &str {
        "team_disband"
    }
    fn description(&self) -> &str {
        "Disband the active team: drop all worker member threads, release the \
         shared task list, and clear the leader's team. No-op message if no team \
         is active. Member conversations are session-scoped and not persisted."
    }
    fn input_schema(&self) -> serde_json::Value {
        crate::tools::schema::<TeamDisbandInput>()
    }
    fn run(
        &self,
        input: serde_json::Value,
        _cancel: CancellationToken,
        _ctx: &dyn ToolContext,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let Ok(_) = serde_json::from_value::<TeamDisbandInput>(input) else {
            return Task::ready(Err("team_disband input parse failed".to_string()));
        };
        let Some(leader) = self.leader.upgrade() else {
            return Task::ready(Err("leader thread dropped".to_string()));
        };
        let Some(team) = leader.read(cx).team().cloned() else {
            return Task::ready(Ok("no active team".to_string()));
        };
        // `disband` clears each member's team back-reference (breaking the
        // strong team↔member cycle) and drops the member threads + subscriptions.
        team.update(cx, |t, cx| t.disband(cx));
        leader.update(cx, |t, cx| t.clear_team(cx));
        Task::ready(Ok("team disbanded".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::ThreadRecord;
    use crate::team::{Member, Team};
    use crate::thread::Thread;
    use gpui::TestAppContext;
    use std::sync::{Arc, Mutex};
    use tokio_util::sync::CancellationToken;

    fn team_with_leader(cx: &mut gpui::App) -> (gpui::Entity<Team>, gpui::Entity<Thread>) {
        let leader = Thread::restore(ThreadRecord::for_test("lead", "/tmp", Vec::new()), None, cx);
        let team = Team::new("squad".into(), leader.downgrade(), cx);
        leader.update(cx, |t, cx| t.set_team(team.clone(), cx));
        (team, leader)
    }

    /// Drive a synchronous tool to completion: build the `Task` (a `Task::ready`
    /// for these tools), then park the executor to await it. Mirrors the
    /// self_info test harness. The `Task` is `'static`, so it moves into the
    /// spawned future with no reference capture.
    fn run_tool(
        cx: &mut TestAppContext,
        tool: &dyn AgentTool,
        input: serde_json::Value,
        ctx: &dyn ToolContext,
    ) -> Result<String, String> {
        let task = cx.update(|cx| tool.run(input, CancellationToken::new(), ctx, cx));
        let out: Arc<Mutex<Option<Result<String, String>>>> = Arc::new(Mutex::new(None));
        let r = out.clone();
        cx.spawn(|_cx| async move {
            *r.lock().unwrap() = Some(task.await);
        })
        .detach();
        cx.run_until_parked();
        out.lock()
            .unwrap()
            .take()
            .expect("tool task did not complete")
    }

    fn snapshot(
        cx: &TestAppContext,
        thread: &gpui::Entity<Thread>,
    ) -> crate::tool::ToolContextSnapshot {
        thread.read_with(cx, |t, _| crate::tool::ToolContextSnapshot::from_thread(t))
    }

    #[test]
    fn task_tools_crud_via_tool_run() {
        crate::agent_def::init();
        let mut cx = TestAppContext::single();
        let (_team, leader) = cx.update(team_with_leader);
        let ctx = snapshot(&cx, &leader);

        // create T1
        let id = run_tool(
            &mut cx,
            &TaskCreateTool,
            serde_json::json!({"subject":"wire UI","description":"tabs"}),
            &ctx,
        )
        .unwrap();
        assert!(id.contains("T1"), "got: {id}");

        // list
        let list = run_tool(&mut cx, &TaskListTool, serde_json::json!({}), &ctx).unwrap();
        assert!(list.contains("wire UI"), "got: {list}");

        // update: assign + in_progress
        let upd = run_tool(
            &mut cx,
            &TaskUpdateTool,
            serde_json::json!({"id":"T1","status":"in_progress","owner":"plan"}),
            &ctx,
        )
        .unwrap();
        assert!(upd.contains("in_progress"), "got: {upd}");
        assert!(upd.contains("plan"), "got: {upd}");

        // get
        let got = run_tool(&mut cx, &TaskGetTool, serde_json::json!({"id":"T1"}), &ctx).unwrap();
        assert!(got.contains("tabs"), "description present: {got}");

        // unassign via owner: null
        let _ = run_tool(
            &mut cx,
            &TaskUpdateTool,
            serde_json::json!({"id":"T1","owner":null}),
            &ctx,
        )
        .unwrap();
        let got = run_tool(&mut cx, &TaskGetTool, serde_json::json!({"id":"T1"}), &ctx).unwrap();
        assert!(got.contains("unassigned"), "owner cleared: {got}");

        // unknown id
        let err = run_tool(
            &mut cx,
            &TaskUpdateTool,
            serde_json::json!({"id":"T9","status":"completed"}),
            &ctx,
        )
        .unwrap_err();
        assert!(err.contains("T9"), "got: {err}");
    }

    #[test]
    fn tools_error_without_team() {
        crate::agent_def::init();
        let mut cx = TestAppContext::single();
        // Bare thread, no team attached.
        let thread = cx.update(|cx| {
            Thread::restore(ThreadRecord::for_test("solo", "/tmp", Vec::new()), None, cx)
        });
        let ctx = snapshot(&cx, &thread);
        let err = run_tool(
            &mut cx,
            &TaskCreateTool,
            serde_json::json!({"subject":"x"}),
            &ctx,
        )
        .unwrap_err();
        assert!(err.contains("no active team"), "got: {err}");
        let err = run_tool(
            &mut cx,
            &SendMessageTool,
            serde_json::json!({"to":"lead","message":"hi"}),
            &ctx,
        )
        .unwrap_err();
        assert!(err.contains("no active team"), "got: {err}");
    }

    #[test]
    fn send_message_delivers_to_idle_member() {
        crate::agent_def::init();
        let mut cx = TestAppContext::single();
        let (team, leader) = cx.update(team_with_leader);
        // Add a member thread (bare, no model — run_turn no-ops).
        let member = cx.update(|cx| {
            Thread::restore(ThreadRecord::for_test("plan", "/tmp", Vec::new()), None, cx)
        });
        cx.update(|cx| {
            team.update(cx, |t, cx| {
                t.insert_member(
                    Member::new("plan".into(), "explorer".into(), member.clone()),
                    cx,
                )
            })
        })
        .unwrap();
        // Attach the team to the member so the recipient's own `agent_label`
        // and `from` are coherent if it ever sends a reply.
        cx.update(|cx| member.update(cx, |m, cx| m.set_team(team.clone(), cx)));

        let ctx = snapshot(&cx, &leader);
        let _ = run_tool(
            &mut cx,
            &SendMessageTool,
            serde_json::json!({"to":"plan","message":"go"}),
            &ctx,
        )
        .unwrap();
        cx.run_until_parked();
        let msgs = member.read_with(&cx, |t, _| t.messages().to_vec());
        assert_eq!(msgs.len(), 1, "member received the message");
    }

    #[test]
    fn shared_tools_names_are_unique() {
        let tools = shared_tools();
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "duplicate tool names: {names:?}");
        for expected in [
            "task_create",
            "task_list",
            "task_update",
            "task_get",
            "send_message",
        ] {
            assert!(names.contains(&expected), "missing {expected}: {names:?}");
        }
    }

    #[test]
    fn leader_tools_exposes_three_management_tools() {
        let cx = TestAppContext::single();
        let leader = cx.update(|cx| {
            Thread::restore(ThreadRecord::for_test("lead", "/tmp", Vec::new()), None, cx)
        });
        let tools = leader_tools(leader.downgrade());
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert!(names.contains(&"team_create"), "got: {names:?}");
        assert!(names.contains(&"team_spawn"), "got: {names:?}");
        assert!(names.contains(&"team_disband"), "got: {names:?}");
    }

    /// The team subscription in `spawn_team_member` forwards a member's
    /// `ToolCallAuthorization` to the leader via `register_child_auth` with a
    /// `<name>::<child_id>` composite id, which re-emits the auth on the leader
    /// under that id so the existing approval overlay prompts; the leader's
    /// `respond_authorization` then routes the decision back. Driving a member
    /// turn to emit its own auth needs a live provider (out of scope for a unit
    /// test), so this exercises the surfacing path directly — the call the
    /// subscription makes.
    #[test]
    fn register_child_auth_surfaces_composite_id_on_leader() {
        crate::agent_def::init();
        let cx = TestAppContext::single();
        let (leader, member) = cx.update(|cx| {
            let leader =
                Thread::restore(ThreadRecord::for_test("lead", "/tmp", Vec::new()), None, cx);
            let member =
                Thread::restore(ThreadRecord::for_test("plan", "/tmp", Vec::new()), None, cx);
            (leader, member)
        });
        let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let cap = captured.clone();
        cx.update(|cx| {
            cx.subscribe(&leader, move |_, e: &ThreadEvent, _| {
                if let ThreadEvent::ToolCallAuthorization { id, .. } = e {
                    cap.lock().unwrap().push(id.clone());
                }
            })
            .detach();
        });

        cx.update(|cx| {
            leader.update(cx, |t, cx| {
                t.register_child_auth(
                    "plan::child1".into(),
                    member.downgrade(),
                    "child1".into(),
                    "write_file".into(),
                    "[plan] write_file /tmp/x".into(),
                    serde_json::json!({}),
                    cx,
                );
            })
        });

        let ids = captured.lock().unwrap().clone();
        assert_eq!(
            ids,
            vec!["plan::child1".to_string()],
            "composite id surfaced"
        );
    }
}
